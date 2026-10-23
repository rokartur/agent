use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// The image file a paste names: terminals paste a dropped file as its path, shell-escaped or quoted.
pub fn pasted_path(text: &str) -> Option<PathBuf> {
    let text = text.trim();
    let quoted = ['\'', '"'].into_iter().find_map(|quote| text.strip_prefix(quote)?.strip_suffix(quote));
    let path = match quoted {
        Some(quoted) => PathBuf::from(quoted),
        None => PathBuf::from(unescape(text)),
    };
    (media_type(&path).is_some() && path.is_file()).then_some(path)
}

/// Undoes shell escaping: `\x` becomes `x` for any `x`.
fn unescape(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => out.extend(chars.next()),
            _ => out.push(ch),
        }
    }
    out
}

/// The Anthropic API refuses images whose base64 data is over 5 MB; base64 carries 3 bytes in 4 chars.
const MAX_IMAGE_BYTES: u64 = (5 << 20) / 4 * 3;

/// Larger files are not read at all: shrinking one would hold it whole in memory.
const MAX_SOURCE_BYTES: u64 = 100 << 20;

/// Reads an image file into an API image block.
pub fn block(path: &Path) -> Result<Value, String> {
    let size = std::fs::metadata(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?.len();
    if size > MAX_SOURCE_BYTES {
        return Err(format!(
            "{} is {}; images over {} are not attached. Shrink it first, e.g. magick in.png -resize 50% out.jpg.",
            path.display(),
            crate::tools::format_size(size as usize),
            crate::tools::format_size(MAX_SOURCE_BYTES as usize)
        ));
    }
    let bytes = std::fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    bytes_block(bytes).map_err(|e| format!("{e}: {}", path.display()))
}

/// Requests with many images are refused when any image side is over 3000 px.
const MAX_IMAGE_SIDE: u32 = 3000;

fn bytes_block(mut bytes: Vec<u8>) -> Result<Value, String> {
    let mut media_type = sniffed_media_type(&bytes).ok_or("Not a PNG, JPEG, GIF or WebP image")?;
    let longest_side = dimensions(&bytes).map(|(width, height)| width.max(height));
    let too_wide = longest_side.is_some_and(|side| side > MAX_IMAGE_SIDE);
    let too_heavy = bytes.len() as u64 > MAX_IMAGE_BYTES;
    if too_wide || too_heavy {
        // A 3000 px PNG photo can still be over the byte limit; JPEG brings it under.
        let format = if too_heavy { "image/jpeg" } else { media_type };
        (media_type, bytes) = shrunk(&bytes, format, longest_side.unwrap_or(MAX_IMAGE_SIDE).min(MAX_IMAGE_SIDE))?;
    }
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!("Still {} after shrinking; the API takes images up to {}", crate::tools::format_size(bytes.len()), crate::tools::format_size(MAX_IMAGE_BYTES as usize)));
    }
    Ok(json!({"type": "image", "source": {"type": "base64", "media_type": media_type, "data": base64(&bytes)}}))
}

/// Shrinks the oversized images in `messages`, so a session saved before the side limit can be sent again.
/// An image that fails to shrink stays as it was; the API's own refusal then names it.
pub fn fit_history(messages: &mut [Value]) {
    for message in messages {
        let Some(blocks) = message["content"].as_array_mut() else { continue };
        for block in blocks {
            for result in block.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
                let _ = fit_block(result);
            }
            let _ = fit_block(block);
        }
    }
}

fn fit_block(block: &mut Value) -> Result<(), String> {
    let Some(data) = block.get("source").and_then(|source| source.get("data")?.as_str()).filter(|_| block.get("type").is_some_and(|kind| kind == "image")) else { return Ok(()) };
    // 64 KiB of base64 reaches the frame header of any JPEG but one with a huge EXIF block.
    let head = unbase64(&data[..data.len().min(1 << 16)]);
    if !head.and_then(|head| dimensions(&head)).is_some_and(|(width, height)| width.max(height) > MAX_IMAGE_SIDE) {
        return Ok(());
    }
    let bytes = unbase64(data).ok_or("An image in the session is not valid base64")?;
    *block = bytes_block(bytes)?;
    Ok(())
}

/// Width and height from the image header, None when the header is not one this reads.
fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let be16 = |at: usize| Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32);
    let le16 = |at: usize| Some(u16::from_le_bytes(bytes.get(at..at + 2)?.try_into().ok()?) as u32);
    let le24 = |at: usize| Some(u32::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?, *bytes.get(at + 2)?, 0]));
    match sniffed_media_type(bytes)? {
        "image/png" => Some((u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?), u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?))),
        "image/gif" => Some((le16(6)?, le16(8)?)),
        "image/webp" => match bytes.get(12..16)? {
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            b"VP8L" => {
                let bits = u32::from_le_bytes(bytes.get(21..25)?.try_into().ok()?);
                Some((1 + (bits & 0x3fff), 1 + ((bits >> 14) & 0x3fff)))
            }
            b"VP8X" => Some((1 + le24(24)?, 1 + le24(27)?)),
            _ => None,
        },
        _ => {
            // JPEG: walk the segments to the frame header (SOF0..SOF15 except DHT, JPG and DAC).
            let mut at = 2;
            loop {
                let marker = *bytes.get(at + 1)?;
                if (0xc0..=0xcf).contains(&marker) && ![0xc4, 0xc8, 0xcc].contains(&marker) {
                    return Some((be16(at + 7)?, be16(at + 5)?));
                }
                at += 2 + be16(at + 2)? as usize;
            }
        }
    }
}

const SHRINK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Scales the image so its longest side is `side` px, with `sips` on macOS, ImageMagick elsewhere. A JPEG stays JPEG,
/// the rest become PNG: sips cannot write WebP, and a GIF keeps only its first frame.
fn shrunk(bytes: &[u8], media_type: &'static str, side: u32) -> Result<(&'static str, Vec<u8>), String> {
    let (media_type, format) = if media_type == "image/jpeg" { ("image/jpeg", "jpeg") } else { ("image/png", "png") };
    let dir = std::env::temp_dir().join(format!("agent-images-{}", std::process::id()));
    crate::process::private_dir(&dir)?;
    let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock is after 1970").as_nanos();
    let input = dir.join(format!("{nonce}-in"));
    let output = dir.join(format!("{nonce}-out.{format}"));
    std::fs::write(&input, bytes).map_err(|e| format!("Cannot write {}: {e}", input.display()))?;
    let side = side.to_string();
    // ImageMagick 6, as Ubuntu ships it, has `convert` and no `magick`.
    let programs: &[&str] = if cfg!(target_os = "macos") { &["sips"] } else { &["magick", "convert"] };
    let mut program = programs[0];
    let mut result = Err(std::io::ErrorKind::NotFound.into());
    for &candidate in programs {
        program = candidate;
        let mut command = crate::process::command(program);
        if program == "sips" {
            command.args(["-s", "format", format, "-Z", &side]).arg(&input).arg("--out").arg(&output);
        } else {
            command.arg(format!("{}[0]", input.display())).args(["-resize", &format!("{side}x{side}")]).arg(&output);
        }
        result = crate::process::output_within(&mut command, SHRINK_TIMEOUT);
        if !result.as_ref().is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) {
            break;
        }
    }
    let shrunk = std::fs::read(&output);
    let _ = std::fs::remove_file(&input);
    let _ = std::fs::remove_file(&output);
    let finished = result.map_err(|e| format!("Cannot run {program} to shrink an image to {side} px: {e}"))?.ok_or(format!("{program} timed out shrinking an image"))?;
    if !finished.status.success() {
        return Err(format!("{program} could not shrink the image: {}", String::from_utf8_lossy(&finished.stderr).trim()));
    }
    let shrunk = shrunk.map_err(|e| format!("Cannot read {}: {e}", output.display()))?;
    Ok((media_type, shrunk))
}

/// The API rejects a media type that disagrees with the bytes, and a `.png` file can hold a JPEG.
fn sniffed_media_type(bytes: &[u8]) -> Option<&'static str> {
    match bytes {
        [0x89, b'P', b'N', b'G', ..] => Some("image/png"),
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [b'G', b'I', b'F', b'8', ..] => Some("image/gif"),
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some("image/webp"),
        _ => None,
    }
}

const CLIPBOARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Reads the clipboard image into an API image block: `osascript` on macOS, `wl-paste` on Wayland, `xclip` on X11.
pub fn from_clipboard() -> Result<Value, String> {
    if cfg!(target_os = "macos") {
        return from_macos_clipboard();
    }
    let (program, args): (&str, &[&str]) =
        if std::env::var_os("WAYLAND_DISPLAY").is_some() { ("wl-paste", &["--type", "image/png"]) } else { ("xclip", &["-selection", "clipboard", "-target", "image/png", "-out"]) };
    // Runs on the UI thread; a clipboard owner that never answers would freeze the TUI.
    let output = crate::process::output_within(crate::process::command(program).args(args), CLIPBOARD_TIMEOUT).map_err(|e| format!("Cannot run {program}: {e}"))?.ok_or("Clipboard read timed out.")?;
    if !output.status.success() || sniffed_media_type(&output.stdout).is_none() {
        return Err("No image in the clipboard".into());
    }
    bytes_block(output.stdout)
}

/// Saves the clipboard image as PNG and reads it into an API image block.
fn from_macos_clipboard() -> Result<Value, String> {
    let path = std::env::temp_dir().join(format!("agent-clipboard-{}.png", std::process::id()));
    // The path goes in as an argument, so a `"` in $TMPDIR cannot end a string literal and run as AppleScript.
    let script = "on run argv\nset f to open for access (POSIX file (item 1 of argv)) with write permission\nset eof f to 0\nwrite (the clipboard as «class PNGf») to f\nclose access f\nend run";
    let output = crate::process::output_within(crate::process::command("osascript").args(["-e", script]).arg(&path), CLIPBOARD_TIMEOUT)
        .map_err(|e| format!("Cannot run osascript: {e}"))?
        .ok_or("Clipboard read timed out.")?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    // -1700 is "can't make into type": the clipboard holds no image. Anything else (a sandbox denial) is shown.
    let block = if output.status.success() {
        block(&path)
    } else if stderr.contains("-1700") {
        Err("No image in the clipboard".into())
    } else {
        Err(format!("osascript: {}", stderr.trim()))
    };
    let _ = std::fs::remove_file(&path);
    block
}

pub fn media_type(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

fn base64(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let mut padded = [0; 4];
        padded[1..=chunk.len()].copy_from_slice(chunk);
        let n = u32::from_be_bytes(padded);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(BASE64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

fn unbase64(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for ch in text.bytes().take_while(|&ch| ch != b'=') {
        let value = match ch {
            b'A'..=b'Z' => ch - b'A',
            b'a'..=b'z' => ch - b'a' + 26,
            b'0'..=b'9' => ch - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        acc = (acc << 6 | value as u32) & 0xffffff;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIXEL: [u8; 70] = [
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
        0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60, 0xf8, 0x5f, 0x0f, 0x00, 0x02, 0x87, 0x01, 0x80, 0xeb, 0x47, 0xba, 0x92, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
        0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn base64_pads_short_chunks() {
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
        assert_eq!(unbase64("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(unbase64("+/8=").unwrap(), [0xfb, 0xff]);
    }

    #[test]
    fn a_dropped_image_path_is_recognised() {
        let dir = std::env::temp_dir().join(format!("agent-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("capture screen (1) & co.png");
        std::fs::write(&file, b"png").unwrap();
        let escaped = file.display().to_string().replace(' ', "\\ ").replace('(', "\\(").replace(')', "\\)").replace('&', "\\&");
        assert_eq!(pasted_path(&escaped), Some(file.clone()));
        assert_eq!(pasted_path(&format!("'{}'", file.display())), Some(file.clone()));
        assert_eq!(pasted_path(&format!("\"{}\"", file.display())), Some(file.clone()));
        assert_eq!(pasted_path("just some text"), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn media_type_follows_the_bytes_not_the_extension() {
        let file = std::env::temp_dir().join(format!("agent-jpeg-as-png-{}.png", std::process::id()));
        std::fs::write(&file, [0xff, 0xd8, 0xff, 0xe0]).unwrap();
        let block = block(&file);
        std::fs::remove_file(&file).unwrap();
        assert_eq!(block.unwrap()["source"]["media_type"], "image/jpeg");
    }

    #[test]
    fn an_image_over_the_api_limit_is_refused_before_reading() {
        let file = std::env::temp_dir().join(format!("agent-big-image-{}.png", std::process::id()));
        std::fs::File::create(&file).unwrap().set_len(MAX_SOURCE_BYTES + 1).unwrap();
        let block = block(&file);
        std::fs::remove_file(&file).unwrap();
        assert!(block.unwrap_err().contains("images over"));
    }

    #[test]
    fn an_oversized_image_in_the_history_is_shrunk_to_the_side_limit() {
        let file = std::env::temp_dir().join(format!("agent-wide-image-{}.png", std::process::id()));
        std::fs::write(&file, PIXEL).unwrap();
        let mut upscale = if cfg!(target_os = "macos") {
            let mut command = crate::process::command("sips");
            command.args(["-z", "1920", "3840"]).arg(&file);
            command
        } else {
            let mut command = crate::process::command("convert");
            command.arg(&file).args(["-resize", "3840x1920!"]).arg(&file);
            command
        };
        assert!(upscale.output().unwrap().status.success());
        let wide = std::fs::read(&file).unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(dimensions(&wide), Some((3840, 1920)));

        let image = |bytes: &[u8]| json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": base64(bytes)}});
        let mut messages = [json!({"role": "user", "content": [{"type": "tool_result", "content": [image(&wide)]}, image(&PIXEL), {"type": "text", "text": "look"}]})];
        fit_history(&mut messages);
        let side = |block: &Value| dimensions(&unbase64(block["source"]["data"].as_str().unwrap()).unwrap());
        assert_eq!(side(&messages[0]["content"][0]["content"][0]), Some((3000, 1500)));
        assert_eq!(messages[0]["content"][1], image(&PIXEL));
        assert_eq!(messages[0]["content"][2], json!({"type": "text", "text": "look"}));
    }

    #[test]
    fn an_image_over_the_byte_limit_is_reencoded_as_jpeg() {
        let mut heavy = PIXEL.to_vec();
        heavy.resize(MAX_IMAGE_BYTES as usize + 1, 0);
        let block = bytes_block(heavy).unwrap();
        assert_eq!(block["source"]["media_type"], "image/jpeg");
        let bytes = unbase64(block["source"]["data"].as_str().unwrap()).unwrap();
        assert_eq!(dimensions(&bytes), Some((1, 1)));
    }
}
