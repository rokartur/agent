use serde_json::{Value, json};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;

const WHISPER_MODEL: &str = "models/ggml-large-v3-turbo-q5_0.bin";
const WHISPER_MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin";
const VAD_MODEL: &str = "models/ggml-silero-v5.1.2.bin";
const VAD_MODEL_URL: &str = "https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v5.1.2.bin";
/// The codes whisper.cpp's `-l` takes, from `g_lang` in its whisper.cpp.
const WHISPER_LANGUAGES: &str = "en zh de es ru ko fr ja pt tr pl ca nl ar sv it id hi fi vi he uk el ms cs ro da hu ta no th ur hr bg lt la mi ml cy sk te fa lv bn sr az sl kn et mk br eu is hy ne mn bs kk sq sw gl mr pa si km sn yo so af oc ka be tg sd gu am yi lo uz fo ht ps tk nn mt sa lb my bo tl mg as tt haw ln ha ba jw su yue";
const DEFAULT_FRAMES: u64 = 8;
const MAX_FRAMES: u64 = 20;
/// Wide enough to read code and slides; Claude scales images over about 1.15 megapixels down anyway.
const FRAME_WIDTH: u32 = 1280;
/// A call with more frames shrinks them to this many frames' pixels: 20 frames cost about 10k tokens, not 25k.
const FULL_WIDTH_FRAMES: u64 = 8;
/// About two to four hours of 720p video in `~/.agents/cache/videos`; the oldest download goes first.
const CACHE_BYTES: u64 = 2 << 30;
/// ffmpeg's scene score between two keyframes that counts as a cut.
const SCENE_CUT: f64 = 0.3;
/// A cut closer than this to the one before is a fade or a flash, not a new shot.
const MIN_SHOT: f64 = 1.0;
/// Subtitle codecs ffmpeg writes out as SRT text; bitmap ones, such as Blu-ray PGS, would need OCR.
const TEXT_SUBTITLES: [&str; 6] = ["subrip", "webvtt", "ass", "ssa", "mov_text", "text"];
/// A longer description goes to the small model: past this, it is mostly sponsors, links and credits.
const DESCRIPTION_CHARS: usize = 1000;
pub const SUMMARY_SYSTEM: &str = "You summarize the description of a video for someone about to watch it. Keep what it says about the content, corrections, and links to sources. Drop sponsor reads, merchandise, social media and channel links, and credits. Reply with only the summary, at most six lines, in the language of the description.";
/// Six lines need about 100; the rest is room for a `small_model` that thinks, as thinking counts against max_tokens.
pub const SUMMARY_MAX_TOKENS: u64 = 2000;

/// A spoken line: start and end in seconds on the video's clock, and its words.
type Line = (f64, f64, String);

/// The whisper.cpp model in `agent_dir`, which /voice uses too.
pub fn whisper_model(agent_dir: &Path) -> Result<PathBuf, String> {
    model_file(agent_dir, WHISPER_MODEL, WHISPER_MODEL_URL)
}

fn model_file(agent_dir: &Path, file: &str, url: &str) -> Result<PathBuf, String> {
    let model = agent_dir.join(file);
    if !model.exists() {
        return Err(format!("No model at {}. Download it: curl -L --create-dirs -o {} {url}", model.display(), model.display()));
    }
    Ok(model)
}

/// The Video tool: the title, chapters and description a video carries, then frames over the span as image blocks,
/// each after the lines spoken around it. The lines come from a subtitle track in the spoken language, else from whisper.cpp.
/// `progress` hears each stage, for the running tool's row; `summarize` shortens a long description with a small model.
pub fn watch(
    input: &Value,
    cwd: &Path,
    agent_dir: &Path,
    abort: &AtomicBool,
    progress: &mut dyn FnMut(String),
    summarize: impl FnOnce(&str) -> Result<String, String> + Send + 'static,
) -> Result<(String, Vec<Value>), String> {
    let source = input["source"].as_str().ok_or("Video needs `source`: a file path or a URL.")?;
    let scratch = Scratch::create()?;
    let downloaded = is_url(source);
    let (file, info) = if downloaded { download(source, &cache_dir(agent_dir)?, &scratch.0, abort, progress)? } else { (super::hashline::resolve(cwd, source), Value::Null) };
    let video = Video { language: info["language"].as_str().and_then(whisper_language), ..probe(file, &scratch.0, abort)? };
    let start = input["start"].as_f64().unwrap_or(0.0).max(0.0);
    let end = input["end"].as_f64().map_or(video.duration, |end| end.min(video.duration));
    if start >= end {
        return Err(format!("The span {}-{} is empty: the video is {} long.", clock(start), clock(end), clock(video.duration)));
    }
    let description = tag(&video.tags, "description").unwrap_or_default().to_string();
    let summary = (description.chars().count() > DESCRIPTION_CHARS).then(|| {
        let description = description.clone();
        std::thread::spawn(move || summarize(&description))
    });
    let span = format!("{}-{}", clock(start), clock(end));
    let mut text = format!("{}, {} long.", video.path.display(), clock(video.duration));
    if downloaded {
        text.push_str("\nKept in the video cache: pass that path as `source` to look again without asking yt-dlp.");
    }
    text.push_str(&about(&video.tags, &video.chapters));
    let count = if video.has_video { input["frames"].as_u64().unwrap_or(DEFAULT_FRAMES).min(MAX_FRAMES) } else { 0 };
    let mut cuts: Vec<f64> = video.chapters.iter().map(|(at, _)| *at).collect();
    if count > 0 {
        progress("finding scene cuts".into());
        cuts.extend(scene_cuts(&video.path, start, end, &scratch.0, abort)?);
    }
    cuts.sort_by(f64::total_cmp);
    let times = frame_times(start, end, count, &cuts);
    let width = frame_width(count);
    let mut frames = Vec::new();
    for (index, &at) in times.iter().enumerate() {
        progress(format!("frame {}/{count}", index + 1));
        frames.push(frame(&video.path, at, width, &scratch.0, abort)?);
    }
    let transcript = transcript(&video, start, end, agent_dir, &scratch.0, abort, progress);
    if summary.as_ref().is_some_and(|thread| !thread.is_finished()) {
        progress("summarizing the description".into());
    }
    text.push_str(&described(&description, summary.map(|thread| thread.join().expect("summarizing does not panic"))));
    text.push_str("\n\n");
    if count > 0 {
        text.push_str(&format!("{count} frames of {span} follow, each after the lines spoken around it.\n"));
    }
    let mut lines = match transcript {
        Ok((lines, from)) => {
            text.push_str(&format!("Transcript of {span} from {from}{}", if lines.is_empty() { ": no speech." } else { "." }));
            lines
        }
        Err(error) => {
            text.push_str(&format!("No transcript: {error}"));
            Vec::new()
        }
    };
    let all: Vec<String> = lines.iter().map(said).collect();
    if let Some(head) = super::cut_head(&all.join("\n"), "video-transcript") {
        text.push_str(&format!(" It stops after line {} of {}; the whole transcript is in {}.", head.lines, head.total, head.path));
        lines.truncate(head.lines);
    }
    Ok((super::truncate_head(text, "video"), interleaved(&times, frames, &lines)))
}

/// A source yt-dlp downloads; anything else is a file path.
pub fn is_url(source: &str) -> bool {
    source.starts_with("https://") || source.starts_with("http://")
}

/// Frames, audio and logs of one call, removed when the call ends.
struct Scratch(PathBuf);

impl Scratch {
    fn create() -> Result<Self, String> {
        crate::process::private_dir(&super::spill_dir())?;
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock is after 1970").as_nanos();
        let dir = super::spill_dir().join(format!("video-{nonce}"));
        crate::process::private_dir(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Downloads and whole-file transcripts, kept across sessions until they pass `CACHE_BYTES`.
fn cache_dir(agent_dir: &Path) -> Result<PathBuf, String> {
    let cache = agent_dir.join("cache");
    std::fs::create_dir_all(&cache).map_err(|e| format!("Cannot create {}: {e}", cache.display()))?;
    let dir = cache.join("videos");
    crate::process::private_dir(&dir)?;
    Ok(dir)
}

/// Downloads at up to 720p into the video cache, named by video id, so a later call finds the file there.
/// yt-dlp writes the title, description, chapters and the creator's subtitles in the spoken language into the file.
/// Returns the file and yt-dlp's info about the video.
fn download(url: &str, cache: &Path, scratch: &Path, abort: &AtomicBool, progress: &mut dyn FnMut(String)) -> Result<(PathBuf, Value), String> {
    progress("looking up the video".into());
    let template = cache.join("%(id)s.%(ext)s");
    let mut command = crate::process::command("yt-dlp");
    command.args(["--no-playlist", "--playlist-items", "1", "-S", "res:720", "--dump-json", "-o"]).arg(&template).arg("--").arg(url);
    let stdout = run(&mut command, scratch, abort, |_| {})?;
    let info_json = stdout.lines().next().unwrap_or_default();
    let info: Value = serde_json::from_str(info_json).map_err(|e| format!("yt-dlp printed no video info for {url}: {e}"))?;
    // `filename` is where the download below lands; a file there is a finished download, as yt-dlp renames only at the end.
    let cached = PathBuf::from(info["filename"].as_str().unwrap_or_default());
    if cached.is_file() {
        return Ok((cached, info));
    }
    progress("downloading".into());
    let info_path = scratch.join("info.json");
    std::fs::write(&info_path, info_json).map_err(|e| format!("Cannot write {}: {e}", info_path.display()))?;
    let mut command = crate::process::command("yt-dlp");
    command
        .arg("--load-info-json")
        .arg(&info_path)
        // --no-mtime: `evict` reads the file's time as the download time, not the upload's.
        .args(["-S", "res:720", "--no-mtime", "--embed-metadata", "--embed-chapters", "--newline", "--progress"])
        .args(["--progress-template", "download:progress %(progress._percent_str)s", "--print", "after_move:filepath", "-o"])
        .arg(&template);
    // Only the spoken language: a video can carry 99 translated tracks, and auto-captions are worse than whisper.
    if let Some(language) = info["language"].as_str().filter(|language| info["subtitles"].get(*language).is_some()) {
        command.args(["--embed-subs", "--sub-format", "vtt", "--sub-langs", language]);
    }
    let stdout = run(&mut command, scratch, abort, |line| {
        if let Some(percent) = line.strip_prefix("progress ") {
            progress(format!("downloading {}", percent.trim()));
        }
    })?;
    let path = PathBuf::from(stdout.lines().last().unwrap_or_default());
    if !path.is_file() {
        return Err(format!("yt-dlp downloaded no file from {url}"));
    }
    evict(cache, CACHE_BYTES, &path)?;
    Ok((path, info))
}

/// Removes the oldest files in `dir` until it holds at most `limit` bytes, all but `kept`.
/// shortcut: oldest by download, not by last look; track use if old videos get watched again often.
fn evict(dir: &Path, limit: u64, kept: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| format!("Cannot read {}: {e}", dir.display()))? {
        let path = entry.map_err(|e| format!("Cannot read {}: {e}", dir.display()))?.path();
        let metadata = std::fs::metadata(&path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
        if metadata.is_file() {
            files.push((metadata.modified().map_err(|e| format!("Cannot read {}: {e}", path.display()))?, metadata.len(), path));
        }
    }
    files.sort();
    let mut total: u64 = files.iter().map(|(_, size, _)| size).sum();
    for (_, size, path) in files {
        if total <= limit {
            break;
        }
        if path != kept {
            std::fs::remove_file(&path).map_err(|e| format!("Cannot remove {}: {e}", path.display()))?;
            total -= size;
        }
    }
    Ok(())
}

/// What ffprobe reads from a video file.
struct Video {
    path: PathBuf,
    duration: f64,
    /// A cover picture in an audio file is not video.
    has_video: bool,
    has_audio: bool,
    /// The stream index and language of a text subtitle track in the spoken language.
    subtitles: Option<(u64, String)>,
    tags: Value,
    /// Start in seconds and title.
    chapters: Vec<(f64, String)>,
    /// The spoken language yt-dlp names, as a whisper.cpp code; whisper guesses from the first 30 s without it.
    language: Option<String>,
}

fn probe(path: PathBuf, scratch: &Path, abort: &AtomicBool) -> Result<Video, String> {
    let mut command = crate::process::command("ffprobe");
    command
        .args(["-v", "error", "-show_entries", "format=duration:format_tags:stream=index,codec_type,codec_name:stream_tags=language:stream_disposition=forced,attached_pic"])
        .args(["-show_chapters", "-of", "json"])
        .arg(&path);
    let probe: Value = serde_json::from_str(&run(&mut command, scratch, abort, |_| {})?).map_err(|e| format!("ffprobe printed no JSON: {e}"))?;
    let duration = probe["format"]["duration"].as_str().and_then(|duration| duration.parse().ok()).ok_or(format!("{} has no duration; is it a video?", path.display()))?;
    let streams = probe["streams"].as_array().map_or(&[][..], Vec::as_slice);
    let has_video = streams.iter().any(|stream| stream["codec_type"] == "video" && stream["disposition"]["attached_pic"] != 1);
    let has_audio = streams.iter().any(|stream| stream["codec_type"] == "audio");
    let chapters = probe["chapters"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|chapter| Some((chapter["start_time"].as_str()?.parse().ok()?, chapter["tags"]["title"].as_str().unwrap_or_default().to_string())))
        .collect();
    Ok(Video { duration, has_video, has_audio, subtitles: spoken_subtitles(streams), tags: probe["format"]["tags"].clone(), chapters, path, language: None })
}

/// A text subtitle track in the audio's language says what is spoken; a track in another language is a translation.
fn spoken_subtitles(streams: &[Value]) -> Option<(u64, String)> {
    let audio = streams.iter().find(|stream| stream["codec_type"] == "audio")?;
    let language = audio["tags"]["language"].as_str().filter(|language| *language != "und")?;
    let track = streams.iter().find(|stream| {
        stream["codec_type"] == "subtitle"
            && stream["tags"]["language"] == language
            && stream["disposition"]["forced"] != 1
            && TEXT_SUBTITLES.contains(&stream["codec_name"].as_str().unwrap_or_default())
    })?;
    Some((track["index"].as_u64()?, language.to_string()))
}

/// The title, author, date and chapters a file carries; yt-dlp writes them into its downloads.
fn about(tags: &Value, chapters: &[(f64, String)]) -> String {
    let mut text = String::new();
    for (label, name) in [("Title", "title"), ("By", "artist"), ("Date", "date")] {
        if let Some(value) = tag(tags, name) {
            text.push_str(&format!("\n{label}: {value}"));
        }
    }
    if !chapters.is_empty() {
        text.push_str("\nChapters:");
        for (at, title) in chapters {
            text.push_str(&format!("\n[{}] {title}", clock(*at)));
        }
    }
    text
}

fn tag<'a>(tags: &'a Value, name: &str) -> Option<&'a str> {
    // Matroska keeps yt-dlp's tags upper case: DESCRIPTION, ARTIST.
    let value = tags.as_object()?.iter().find(|(key, _)| key.eq_ignore_ascii_case(name))?.1.as_str()?.trim();
    (!value.is_empty()).then_some(value)
}

/// A short description as it is. A long one as the small model's `summary`, or its head when that failed,
/// with the path of a file holding all of it.
fn described(description: &str, summary: Option<Result<String, String>>) -> String {
    if description.is_empty() {
        return String::new();
    }
    let Some(summary) = summary else { return format!("\nDescription: {description}") };
    let path = super::spill(description, "video-description");
    match summary {
        Ok(summary) => format!("\nDescription, summarized: {}\nThe whole description is in {path}.", summary.trim()),
        Err(error) => {
            let head: String = description.chars().take(DESCRIPTION_CHARS).collect();
            format!("\nDescription, cut short because summarizing failed ({error}): {head}\nThe whole description is in {path}.")
        }
    }
}

/// Scene cuts in the span, judged on keyframes only: 0.25 s for 11 minutes of 720p, where every frame takes 16 s.
fn scene_cuts(file: &Path, start: f64, end: f64, scratch: &Path, abort: &AtomicBool) -> Result<Vec<f64>, String> {
    let mut command = crate::process::command("ffmpeg");
    command.args(["-loglevel", "error", "-skip_frame", "nokey", "-ss", &format!("{start:.3}"), "-to", &format!("{end:.3}"), "-i"]).arg(file).args([
        "-an",
        "-vf",
        &format!("scale=320:-2,select='gt(scene,{SCENE_CUT})',metadata=print:file=-"),
        "-f",
        "null",
        "-",
    ]);
    let stdout = run(&mut command, scratch, abort, |_| {})?;
    // After an input `-ss`, frame times count from `start`.
    Ok(stdout.split("pts_time:").skip(1).filter_map(|rest| rest.split_whitespace().next()?.parse::<f64>().ok()).map(|at| start + at).collect())
}

/// `count` frame times over the span. When there are no more shots between `cuts` than frames, every shot gets one
/// and the rest go where frames sit farthest apart, so a slide shows once instead of in three near-same frames.
/// With more shots than frames, each equal slice of the span shows the shot that fills most of it.
fn frame_times(start: f64, end: f64, count: u64, cuts: &[f64]) -> Vec<f64> {
    let mut bounds = vec![start];
    for &cut in cuts {
        if cut > bounds[bounds.len() - 1] + MIN_SHOT && cut < end - MIN_SHOT {
            bounds.push(cut);
        }
    }
    bounds.push(end);
    let shots: Vec<(f64, f64)> = bounds.windows(2).map(|pair| (pair[0], pair[1])).collect();
    if shots.len() as u64 > count {
        let step = (end - start) / count as f64;
        let mut times = Vec::new();
        for index in 0..count {
            let (from, to) = (start + index as f64 * step, start + (index + 1) as f64 * step);
            let overlaps = shots.iter().map(|&(shot_start, shot_end)| (shot_start.max(from), shot_end.min(to)));
            let (a, b) = overlaps.max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0))).expect("a span has a shot");
            times.push((a + b) / 2.0);
        }
        return times;
    }
    let mut given = vec![1_u32; shots.len()];
    for _ in shots.len() as u64..count {
        let spacing = |shot: usize| (shots[shot].1 - shots[shot].0) / given[shot] as f64;
        let widest = (0..shots.len()).max_by(|&a, &b| spacing(a).total_cmp(&spacing(b))).expect("a span has a shot");
        given[widest] += 1;
    }
    let mut times = Vec::new();
    for (&(from, to), frames) in shots.iter().zip(given) {
        times.extend((0..frames).map(|index| from + (index as f64 + 0.5) * (to - from) / frames as f64));
    }
    times
}

/// Image tokens grow with pixels, so `count` frames share the pixels of `FULL_WIDTH_FRAMES` full-width ones.
fn frame_width(count: u64) -> u32 {
    let share = FULL_WIDTH_FRAMES as f64 / count.max(FULL_WIDTH_FRAMES) as f64;
    (FRAME_WIDTH as f64 * share.sqrt()) as u32 / 2 * 2
}

fn frame(file: &Path, at: f64, width: u32, scratch: &Path, abort: &AtomicBool) -> Result<Value, String> {
    let path = scratch.join(format!("frame-{at}.jpg"));
    let mut command = crate::process::command("ffmpeg");
    command.args(["-loglevel", "error", "-ss", &format!("{at:.3}"), "-i"]).arg(file).args(["-frames:v", "1", "-vf", &format!("scale='min({width},iw)':-2"), "-q:v", "3", "-y"]).arg(&path);
    run(&mut command, scratch, abort, |_| {})?;
    crate::image::block(&path)
}

/// The lines spoken in the span and where they come from: a subtitle track in the spoken language, else whisper.cpp.
/// A whole-file whisper run is kept in the video cache, so a closer look at a part reads it instead of transcribing again.
fn transcript(video: &Video, start: f64, end: f64, agent_dir: &Path, scratch: &Path, abort: &AtomicBool, progress: &mut dyn FnMut(String)) -> Result<(Vec<Line>, String), String> {
    let within = |lines: Vec<Line>| -> Vec<Line> { lines.into_iter().filter(|line| line.1 > start && line.0 < end).collect() };
    if let Some((stream, language)) = &video.subtitles {
        progress("reading subtitles".into());
        let mut command = crate::process::command("ffmpeg");
        command.args(["-loglevel", "error", "-i"]).arg(&video.path).args(["-map", &format!("0:{stream}"), "-f", "srt", "-"]);
        return Ok((within(srt(&run(&mut command, scratch, abort, |_| {})?)), format!("the file's {language} subtitles")));
    }
    if !video.has_audio {
        return Err("the video has no audio track".into());
    }
    let kept = kept_transcript(&video.path, &cache_dir(agent_dir)?)?;
    if let Ok(json) = std::fs::read_to_string(&kept) {
        let lines = serde_json::from_str(&json).map_err(|e| format!("{} is not a transcript: {e}", kept.display()))?;
        return Ok((within(lines), "whisper.cpp, made earlier".into()));
    }
    let lines = whisper(video, start, end, agent_dir, scratch, abort, progress)?;
    if start == 0.0 && end == video.duration {
        std::fs::write(&kept, serde_json::to_string(&lines).expect("lines are plain data")).map_err(|e| format!("Cannot write {}: {e}", kept.display()))?;
    }
    Ok((lines, "whisper.cpp".into()))
}

/// Where the video cache keeps the whole-file transcript of `file`, named by its path, size and change time.
fn kept_transcript(file: &Path, cache: &Path) -> Result<PathBuf, String> {
    let metadata = std::fs::metadata(file).map_err(|e| format!("Cannot read {}: {e}", file.display()))?;
    let mut hasher = DefaultHasher::new();
    (file, metadata.len(), metadata.modified().ok()).hash(&mut hasher);
    Ok(cache.join(format!("transcript-{:016x}.json", hasher.finish())))
}

fn whisper(video: &Video, start: f64, end: f64, agent_dir: &Path, scratch: &Path, abort: &AtomicBool, progress: &mut dyn FnMut(String)) -> Result<Vec<Line>, String> {
    let model = whisper_model(agent_dir)?;
    let vad = model_file(agent_dir, VAD_MODEL, VAD_MODEL_URL)?;
    progress("extracting audio".into());
    let wav = scratch.join("audio.wav");
    let mut command = crate::process::command("ffmpeg");
    command
        .args(["-loglevel", "error", "-ss", &format!("{start:.3}"), "-to", &format!("{end:.3}"), "-i"])
        .arg(&video.path)
        .args(["-vn", "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", "-y"])
        .arg(&wav);
    run(&mut command, scratch, abort, |_| {})?;
    progress("transcribing".into());
    let mut command = crate::process::command("whisper-cli");
    command.args(["-l", video.language.as_deref().unwrap_or("auto"), "-np", "-m"]).arg(&model).arg("-f").arg(&wav);
    // Silero VAD cuts lines at sentence ends, on the creator's subtitle timing; without it lines split mid-sentence, up to 3 s off. 6% slower.
    command.arg("--vad").arg("-vm").arg(&vad);
    let stdout = run(&mut command, scratch, abort, |line| {
        if let Some((at, ..)) = segment(line) {
            progress(format!("transcribing {} / {}", clock(at), clock(end - start)));
        }
    })?;
    Ok(whisper_lines(&stdout, start))
}

/// whisper.cpp's code for a language yt-dlp names, such as `en` for `en-US`.
fn whisper_language(language: &str) -> Option<String> {
    let code = language.split('-').next()?;
    WHISPER_LANGUAGES.split(' ').any(|known| known == code).then(|| code.to_string())
}

/// whisper-cli's `[00:01:02.500 --> 00:01:05.000]   text` rows, moved by `offset` to the video's clock.
fn whisper_lines(stdout: &str, offset: f64) -> Vec<Line> {
    stdout.lines().filter_map(segment).filter(|(.., text)| !text.is_empty()).map(|(from, to, text)| (offset + from, offset + to, text.to_string())).collect()
}

fn segment(line: &str) -> Option<(f64, f64, &str)> {
    let (times, text) = line.strip_prefix('[')?.split_once(']')?;
    let (from, to) = times.split_once(" --> ")?;
    Some((seconds(from)?, seconds(to)?, text.trim()))
}

/// SRT cues as lines; ffmpeg's WebVTT conversion leaves `\h` for a hard space.
fn srt(text: &str) -> Vec<Line> {
    let mut lines: Vec<Line> = Vec::new();
    let mut in_cue = false;
    for row in text.lines() {
        let times = row.split_once(" --> ").and_then(|(from, to)| Some((seconds(from)?, seconds(to.split_whitespace().next()?)?)));
        if let Some((from, to)) = times {
            lines.push((from, to, String::new()));
            in_cue = true;
        } else if row.trim().is_empty() {
            in_cue = false;
        } else if in_cue && let Some(line) = lines.last_mut() {
            line.2 = format!("{} {}", line.2, row.replace("\\h", " "));
        }
    }
    for line in &mut lines {
        line.2 = line.2.split_whitespace().collect::<Vec<_>>().join(" ");
    }
    lines.retain(|line| !line.2.is_empty());
    lines
}

/// `01:02:03.500`, or SRT's `01:02:03,500`, in seconds.
fn seconds(time: &str) -> Option<f64> {
    let mut seconds = 0.0;
    for part in time.trim().replace(',', ".").split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(seconds)
}

/// Each frame after a text with its time and the lines that start before halfway to the next frame.
/// Without frames, all lines make one text.
fn interleaved(times: &[f64], frames: Vec<Value>, lines: &[Line]) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut lines = lines.iter().peekable();
    for (index, (&at, frame)) in times.iter().zip(frames).enumerate() {
        let until = times.get(index + 1).map_or(f64::INFINITY, |next| (at + next) / 2.0);
        let mut text = format!("Frame {}/{} at {}", index + 1, times.len(), clock(at));
        while let Some(line) = lines.next_if(|line| line.0 < until) {
            text.push('\n');
            text.push_str(&said(line));
        }
        blocks.push(json!({"type": "text", "text": text}));
        blocks.push(frame);
    }
    let rest: Vec<String> = lines.map(said).collect();
    if !rest.is_empty() {
        blocks.push(json!({"type": "text", "text": rest.join("\n")}));
    }
    blocks
}

fn said(line: &Line) -> String {
    format!("[{}] {}", clock(line.0), line.2)
}

fn clock(seconds: f64) -> String {
    let total = seconds as u64;
    let (hours, minutes, seconds) = (total / 3600, total / 60 % 60, total % 60);
    if hours > 0 { format!("{hours}:{minutes:02}:{seconds:02}") } else { format!("{minutes}:{seconds:02}") }
}

/// Runs `command`, handing `on_line` each stdout line as it arrives; stderr goes to a file so a failure can quote it.
fn run(command: &mut Command, scratch: &Path, abort: &AtomicBool, mut on_line: impl FnMut(&str)) -> Result<String, String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let log_path = scratch.join(format!("{program}.log"));
    let log = std::fs::File::create(&log_path).map_err(|e| format!("Cannot create {}: {e}", log_path.display()))?;
    let package = match program.as_str() {
        "ffprobe" => "ffmpeg",
        "whisper-cli" => "whisper-cpp",
        other => other,
    };
    let install = if cfg!(windows) { format!("install {package} and put it on PATH") } else { format!("brew install {package}") };
    let child = command.stdout(Stdio::piped()).stderr(log).spawn().map_err(|e| format!("Cannot run {program}: {e}. Install it: {install}"))?;
    let mut stdout = Vec::new();
    let mut handed = 0;
    let (status, killed) = super::wait_streaming(child, abort, None, |chunk| {
        stdout.extend_from_slice(chunk);
        while let Some(newline) = stdout[handed..].iter().position(|&byte| byte == b'\n') {
            on_line(&String::from_utf8_lossy(&stdout[handed..handed + newline]));
            handed += newline + 1;
        }
    })?;
    if killed.is_some() {
        return Err(crate::agent::ABORTED.into());
    }
    if !status.success() {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        let log = log.trim();
        return Err(format!("{program} failed: {}", &log[log.floor_char_boundary(log.len().saturating_sub(1000))..]));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_without_cuts_sit_in_the_middle_of_equal_slices() {
        assert_eq!(frame_times(10.0, 50.0, 4, &[]), [15.0, 25.0, 35.0, 45.0]);
        assert!(frame_times(0.0, 9.0, 0, &[]).is_empty());
    }

    #[test]
    fn each_shot_gets_a_frame_and_the_rest_go_to_the_longest() {
        assert_eq!(frame_times(0.0, 100.0, 3, &[20.0]), [10.0, 40.0, 80.0]);
        // A cut within a second of the last one is a flash, not a shot.
        assert_eq!(frame_times(0.0, 100.0, 3, &[20.0, 20.5, 99.5]), [10.0, 40.0, 80.0]);
    }

    #[test]
    fn with_more_shots_than_frames_each_slice_shows_the_shot_that_fills_it_most() {
        assert_eq!(frame_times(0.0, 100.0, 2, &[10.0, 20.0, 30.0, 55.0]), [40.0, 77.5]);
    }

    #[test]
    fn each_frame_follows_the_lines_spoken_around_it() {
        let text = |text: &str| json!({"type": "text", "text": text});
        let lines = [(1.0, 2.0, "a".to_string()), (6.0, 7.0, "b".to_string()), (14.0, 15.0, "c".to_string())];
        let blocks = interleaved(&[5.0, 15.0], vec![json!("frame 1"), json!("frame 2")], &lines);
        assert_eq!(blocks, [text("Frame 1/2 at 0:05\n[0:01] a\n[0:06] b"), json!("frame 1"), text("Frame 2/2 at 0:15\n[0:14] c"), json!("frame 2")]);
        assert_eq!(interleaved(&[], Vec::new(), &lines), [text("[0:01] a\n[0:06] b\n[0:14] c")]);
    }

    #[test]
    fn whisper_rows_move_to_the_video_clock() {
        let stdout = "\n[00:00:00.000 --> 00:00:02.500]   The quick brown fox.\n[00:59:58.500 --> 01:00:01.000]   Then it ran.\n[00:01:00.000 --> 00:01:01.000]  \nwhisper_print_timings: total\n";
        assert_eq!(whisper_lines(stdout, 75.0), [(75.0, 77.5, "The quick brown fox.".to_string()), (3673.5, 3676.0, "Then it ran.".to_string())]);
    }

    #[test]
    fn subtitle_cues_become_lines() {
        let srt_text = "1\n00:00:00,500 --> 00:00:04,250\nVery recently\\h\nyour body\\h\\h\n\n2\n00:01:04,500 --> 00:01:10,000 X1:0\n<i>the</i> most\n\n3\n00:01:11,000 --> 00:01:12,000\n\n";
        assert_eq!(srt(srt_text), [(0.5, 4.25, "Very recently your body".to_string()), (64.5, 70.0, "<i>the</i> most".to_string())]);
    }

    #[test]
    fn only_a_text_subtitle_track_in_the_spoken_language_counts_as_speech() {
        let stream = |index: u64, kind: &str, codec: &str, language: &str, forced: u64| json!({"index": index, "codec_type": kind, "codec_name": codec, "tags": {"language": language}, "disposition": {"forced": forced}});
        let streams = [
            stream(0, "video", "av1", "", 0),
            stream(1, "audio", "opus", "eng", 0),
            stream(2, "subtitle", "subrip", "pol", 0),
            stream(3, "subtitle", "subrip", "eng", 1),
            stream(4, "subtitle", "hdmv_pgs_subtitle", "eng", 0),
            stream(5, "subtitle", "webvtt", "eng", 0),
        ];
        assert_eq!(spoken_subtitles(&streams), Some((5, "eng".to_string())));
        assert_eq!(spoken_subtitles(&[stream(1, "audio", "opus", "und", 0), stream(2, "subtitle", "subrip", "und", 0)]), None);
    }

    #[test]
    fn the_text_names_the_title_author_and_chapters() {
        let tags = json!({"title": "Viruses", "ARTIST": "Kurzgesagt", "DESCRIPTION": "Tiny.", "ENCODER": "Lavf63"});
        let text = about(&tags, &[(0.0, "Start".into()), (83.0, "You Are an Ecosystem".into())]);
        assert_eq!(text, "\nTitle: Viruses\nBy: Kurzgesagt\nChapters:\n[0:00] Start\n[1:23] You Are an Ecosystem");
        assert_eq!(about(&Value::Null, &[]), "");
    }

    #[test]
    fn a_long_description_comes_summarized_with_the_whole_one_in_a_file() {
        assert_eq!(described("", None), "");
        assert_eq!(described("Tiny.\nEverywhere.", None), "\nDescription: Tiny.\nEverywhere.");
        let long = "word ".repeat(DESCRIPTION_CHARS);
        let summarized = described(&long, Some(Ok(" About words.\n".into())));
        let (summary, path) = summarized.split_once("\nThe whole description is in ").unwrap();
        assert_eq!(summary, "\nDescription, summarized: About words.");
        assert_eq!(std::fs::read_to_string(path.trim_end_matches('.')).unwrap(), long);
        let cut = described(&long, Some(Err("429 rate limited".into())));
        assert!(cut.starts_with(&format!("\nDescription, cut short because summarizing failed (429 rate limited): {}\nThe whole", &long[..DESCRIPTION_CHARS])), "{cut}");
    }

    #[test]
    fn a_kept_whole_file_transcript_answers_a_closer_look_without_whisper() {
        let dir = std::env::temp_dir().join(format!("agent-video-kept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("talk.mp4");
        std::fs::write(&path, "not a real video").unwrap();
        let kept = kept_transcript(&path, &cache_dir(&dir).unwrap()).unwrap();
        let lines = [(0.0, 9.0, "intro"), (9.0, 12.0, "spans the start"), (15.0, 18.0, "inside"), (20.0, 25.0, "after")];
        std::fs::write(&kept, serde_json::to_string(&lines).unwrap()).unwrap();
        let video = Video { path: path.clone(), duration: 30.0, has_video: true, has_audio: true, subtitles: None, tags: Value::Null, chapters: Vec::new(), language: None };
        let mut stages = Vec::new();
        let result = transcript(&video, 10.0, 20.0, &dir, &dir, &AtomicBool::new(false), &mut |stage| stages.push(stage));
        std::fs::remove_dir_all(&dir).unwrap();
        let (lines, from) = result.unwrap();
        assert_eq!(lines, [(9.0, 12.0, "spans the start".to_string()), (15.0, 18.0, "inside".to_string())]);
        assert_eq!((from.as_str(), stages.len()), ("whisper.cpp, made earlier", 0));
    }

    #[test]
    fn clock_shows_hours_only_when_there_are_some() {
        assert_eq!(clock(5.9), "0:05");
        assert_eq!(clock(754.0), "12:34");
        assert_eq!(clock(3723.0), "1:02:03");
    }

    #[test]
    fn whisper_hears_the_language_yt_dlp_names_when_it_knows_it() {
        assert_eq!(["en-US", "pl", "yue", "iw", ""].map(whisper_language), [Some("en".into()), Some("pl".into()), Some("yue".into()), None, None]);
    }

    #[test]
    fn the_oldest_files_leave_the_cache_first_but_never_the_new_download() {
        let dir = std::env::temp_dir().join(format!("agent-video-evict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let epoch = std::time::SystemTime::UNIX_EPOCH;
        for (name, age) in [("old", 3), ("middle", 2), ("new", 1)] {
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_len(10).unwrap();
            file.set_modified(epoch + std::time::Duration::from_secs(1000 - age)).unwrap();
        }
        let left = |dir: &Path| {
            let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().file_name().into_string().unwrap()).collect();
            names.sort();
            names
        };
        evict(&dir, 25, &dir.join("old")).unwrap();
        let kept_old = left(&dir);
        evict(&dir, 15, &dir.join("new")).unwrap();
        let kept_new = left(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!((kept_old, kept_new), (vec!["new".to_string(), "old".to_string()], vec!["new".to_string()]));
    }

    #[test]
    fn frames_past_the_budget_shrink_to_share_its_pixels() {
        assert_eq!((frame_width(1), frame_width(8), frame_width(20)), (1280, 1280, 808));
        let pixels = |count: u64| count * u64::from(frame_width(count)).pow(2);
        assert!(pixels(20) <= pixels(8) && pixels(20) > pixels(8) * 9 / 10);
    }

    #[test]
    fn a_missing_model_names_the_download() {
        let error = whisper_model(Path::new("/nonexistent/.agents")).unwrap_err();
        assert!(error.starts_with("No model at /nonexistent/.agents") && error.contains(WHISPER_MODEL_URL), "{error}");
    }

    /// The whole tool on a clip of macOS `say` speech, with the whisper model /voice uses: whisper once, then the kept
    /// transcript for a closer look, then a subtitle track in the spoken language instead of whisper.
    #[test]
    #[ignore = "needs macOS, ffmpeg, whisper-cli and ~/.agents/models; run with --ignored"]
    fn a_local_clip_comes_back_as_frames_and_a_transcript() {
        let dir = std::env::temp_dir().join(format!("agent-video-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let speech = dir.join("speech.aiff");
        assert!(crate::process::command("say").arg("-o").arg(&speech).arg("The quick brown fox jumps over the lazy dog.").status().unwrap().success());
        let metadata = dir.join("metadata.txt");
        std::fs::write(
            &metadata,
            format!(";FFMETADATA1\ntitle=Fox clip\ndescription={}\n[CHAPTER]\nTIMEBASE=1/1\nSTART=0\nEND=5\ntitle=Intro\n[CHAPTER]\nTIMEBASE=1/1\nSTART=5\nEND=10\ntitle=Outro\n", "fox ".repeat(300)),
        )
        .unwrap();
        let ffmpeg = |args: &[&str]| assert!(crate::process::command("ffmpeg").current_dir(&dir).args(["-loglevel", "error"]).args(args).status().unwrap().success());
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=640x360:rate=10:duration=10",
            "-i",
            "speech.aiff",
            "-i",
            "metadata.txt",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map_metadata",
            "2",
            "-map_chapters",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-y",
            "clip.mp4",
        ]);
        std::fs::write(dir.join("subtitles.srt"), "1\n00:00:00,000 --> 00:00:02,500\nThe quick brown fox.\n").unwrap();
        ffmpeg(&[
            "-i",
            "clip.mp4",
            "-i",
            "subtitles.srt",
            "-map",
            "0",
            "-map",
            "1",
            "-c",
            "copy",
            "-c:s",
            "mov_text",
            "-metadata:s:a:0",
            "language=eng",
            "-metadata:s:s:0",
            "language=eng",
            "-y",
            "subtitled.mp4",
        ]);
        let agent_dir = PathBuf::from(std::env::var("HOME").unwrap()).join(".agents");
        let look = |input: Value| {
            let mut stages = Vec::new();
            let summarize = |description: &str| Ok(format!("{}...", &description[..7]));
            let (text, blocks) = watch(&input, &dir, &agent_dir, &AtomicBool::new(false), &mut |stage| stages.push(stage), summarize).unwrap();
            let texts: Vec<&str> = blocks.iter().filter_map(|block| block["text"].as_str()).collect();
            (text, texts.join("\n"), blocks.iter().filter(|block| block["type"] == "image").count(), stages)
        };
        let whole = look(json!({"source": "clip.mp4", "frames": 3}));
        let closer = look(json!({"source": "clip.mp4", "start": 1, "end": 4, "frames": 0}));
        let subtitled = look(json!({"source": "subtitled.mp4", "frames": 1}));
        std::fs::remove_file(kept_transcript(&dir.join("clip.mp4"), &cache_dir(&agent_dir).unwrap()).unwrap()).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();

        let (text, said, images, stages) = whole;
        assert!(text.contains("Title: Fox clip") && text.contains("[0:05] Outro") && text.contains("Description, summarized: fox fox...") && text.contains("from whisper.cpp."), "{text}");
        assert_eq!(images, 3);
        assert!(said.starts_with("Frame 1/3 at 0:0") && said.to_lowercase().contains("brown fox"), "{said}");
        assert_eq!(stages[..5], ["finding scene cuts", "frame 1/3", "frame 2/3", "frame 3/3", "extracting audio"]);
        assert!(stages.iter().any(|stage| stage.starts_with("transcribing 0:0")), "{stages:?}");

        let (text, said, images, stages) = closer;
        assert!(text.contains("from whisper.cpp, made earlier") && said.to_lowercase().contains("brown fox"), "{text}\n{said}");
        assert_eq!(images, 0);
        assert!(stages.iter().all(|stage| stage == "summarizing the description"), "{stages:?}");

        let (text, said, _, stages) = subtitled;
        assert!(text.contains("from the file's eng subtitles") && said.contains("[0:00] The quick brown fox."), "{text}\n{said}");
        assert_eq!(stages, ["finding scene cuts", "frame 1/1", "reading subtitles"]);
    }
}
