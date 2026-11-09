//! Native ponytail and caveman: the host tracks the level in settings and injects the
//! official rules, downloaded from each project's GitHub repo and cached in ~/.agents/modes.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const PONYTAIL_LEVELS: [&str; 4] = ["off", "lite", "full", "ultra"];
pub const CAVEMAN_LEVELS: [&str; 4] = ["off", "full", "ultra", "wenyan"];
pub const PONYTAIL_BANNER: &str = "PONYTAIL MODE ACTIVE — level: ";
pub const CAVEMAN_BANNER: &str = "CAVEMAN MODE ACTIVE — mode: ";
/// Claude Code's built-in `outputStyle` names.
pub const OUTPUT_STYLES: [&str; 5] = ["default", "Proactive", "Concise", "Explanatory", "Learning"];
// Claude Code 2.1.309's built-in style prompts, word for word.
const OUTPUT_STYLE_PROMPTS: [(&str, &str); 4] = [
    ("Proactive", include_str!("output-styles/Proactive.md")),
    ("Concise", include_str!("output-styles/Concise.md")),
    ("Explanatory", include_str!("output-styles/Explanatory.md")),
    ("Learning", include_str!("output-styles/Learning.md")),
];
// Pinned to commits: the rules go into every system prompt, so upstream changes land only through a reviewed bump here.
const PONYTAIL_URL: &str = "https://raw.githubusercontent.com/DietrichGebert/ponytail/09c434ab73ca43761c5d8b0c5d341926c2fba710/skills/ponytail/SKILL.md";
const CAVEMAN_SKILLS_URL: &str = "https://raw.githubusercontent.com/JuliusBrussee/caveman/bad48a0e1df55e05e3f52249afa35ecc1909ee76/skills";
/// Blocks startup on first run and the TUI on a level change.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(10);

/// Downloads the rules the active levels need: always with `refresh`, otherwise only when missing.
/// A failed download keeps the old copy; a missing copy that cannot be downloaded, or a download that cannot be saved, is an error.
pub fn fetch(home: &Path, ponytail: &str, caveman: &str, refresh: bool) -> Result<(), String> {
    let mut wanted = Vec::new();
    if ponytail != "off" {
        wanted.push(("ponytail", PONYTAIL_URL.to_string()));
    }
    if let Some(skill) = caveman_skill(caveman) {
        wanted.push((skill, format!("{CAVEMAN_SKILLS_URL}/{skill}/SKILL.md")));
    }
    for (skill, url) in wanted {
        let path = cache_path(home, skill);
        let cached = path.exists();
        if cached && !refresh {
            continue;
        }
        match download(&url) {
            Ok(rules) => {
                fs::create_dir_all(path.parent().expect("cache_path has a parent")).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
                // The startup refresh runs while the agent reads the rules; a rename never shows a half-written file.
                // Per-process name: subagents refresh too, and a shared partial could be truncated mid-rename.
                let partial = path.with_extension(format!("{}.partial", std::process::id()));
                fs::write(&partial, rules).map_err(|e| format!("Cannot write {}: {e}", partial.display()))?;
                fs::rename(&partial, &path).map_err(|e| {
                    let _ = fs::remove_file(&partial);
                    format!("Cannot move {} to {}: {e}", partial.display(), path.display())
                })?;
            }
            Err(e) if !cached => return Err(format!("Cannot download the {skill} rules from {url}: {e}")),
            Err(_) => {} // any failed download, offline or upstream: the cached copy still works
        }
    }
    Ok(())
}

/// The system prompt section for the active levels, built from the cached rules, and the output style.
pub fn prompt(home: &Path, ponytail: &str, caveman: &str, output_style: &str) -> String {
    let mut sections = Vec::new();
    if let Some((_, style)) = OUTPUT_STYLE_PROMPTS.iter().find(|(name, _)| *name == output_style) {
        sections.push(style.trim_end().to_string());
    }
    if ponytail != "off"
        && let Some(rules) = read(home, "ponytail")
    {
        sections.push(format!("<ponytail>\n{PONYTAIL_BANNER}{ponytail}\n\n{}\n</ponytail>", only_level(body(&rules), ponytail)));
    }
    if let Some(skill) = caveman_skill(caveman)
        && let Some(rules) = read(home, skill)
    {
        sections.push(format!("<caveman>\n{CAVEMAN_BANNER}{skill}\n\n{}\n</caveman>", body(&rules)));
    }
    sections.join("\n\n")
}

/// What a typed message turns off, as (ponytail, caveman): the official "stop ponytail",
/// "stop caveman" and "normal mode" phrases.
pub fn stopped_by(text: &str) -> (bool, bool) {
    let phrase = text.trim().trim_end_matches(['.', '!', '?']).to_lowercase();
    (phrase == "stop ponytail" || phrase == "normal mode", phrase == "stop caveman" || phrase == "normal mode")
}

/// caveman ships each level as its own skill; these are its legacy level names.
fn caveman_skill(level: &str) -> Option<&'static str> {
    match level {
        "full" => Some("caveman"),
        "ultra" => Some("ultracave"),
        "wenyan" => Some("megacave"),
        _ => None,
    }
}

fn cache_path(home: &Path, skill: &str) -> PathBuf {
    home.join(".agents/modes").join(format!("{skill}.md"))
}

fn read(home: &Path, skill: &str) -> Option<String> {
    fs::read_to_string(cache_path(home, skill)).ok()
}

fn download(url: &str) -> Result<String, String> {
    let http: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(DOWNLOAD_TIMEOUT)).build().into();
    let rules = http.get(url).call().map_err(|e| e.to_string())?.body_mut().read_to_string().map_err(|e| e.to_string())?;
    if !rules.starts_with("---") {
        return Err("the response is not a SKILL.md".into());
    }
    Ok(rules)
}

fn body(skill: &str) -> &str {
    let Some(frontmatter) = skill.strip_prefix("---") else { return skill };
    let mut offset = 0;
    for line in frontmatter.split_inclusive('\n') {
        offset += line.len();
        if line.trim_end() == "---" {
            return frontmatter[offset..].trim_start();
        }
    }
    skill
}

/// ponytail's SKILL.md lists every level; keep only the active level's table row and examples.
fn only_level(body: &str, level: &str) -> String {
    let mut kept = Vec::new();
    for line in body.lines() {
        // [1..] skips "off", which has no row of its own.
        if level_label(line).is_some_and(|label| PONYTAIL_LEVELS[1..].contains(&label.as_str()) && label != level) {
            continue;
        }
        kept.push(line);
    }
    kept.join("\n")
}

/// The level a table row (`| **Lite** |`) or an example (`- Lite: "..."`) belongs to, lowercased.
fn level_label(line: &str) -> Option<String> {
    if let Some((label, _)) = line.strip_prefix("| **").and_then(|rest| rest.split_once("**")) {
        return Some(label.trim().to_lowercase());
    }
    let (label, value) = line.strip_prefix('-')?.split_once(':')?;
    value.trim_start().starts_with('"').then(|| label.trim().to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ponytail_keeps_only_the_active_level() {
        let body = "Rules\n| **lite** | a |\n| **ultra** | b |\n- lite: \"x\"\n- ultra: \"y\"\n- Full: prose stays";
        assert_eq!(only_level(body, "ultra"), "Rules\n| **ultra** | b |\n- ultra: \"y\"\n- Full: prose stays");
    }

    #[test]
    fn frontmatter_ends_at_a_fence_line_only() {
        assert_eq!(body("---\ndescription: lazy --- but complete\n---\nRules"), "Rules");
        assert_eq!(body("---\n---x: y\n---\r\nRules"), "Rules");
    }

    #[test]
    fn prompt_uses_cached_rules_for_active_levels_only() {
        let home = std::env::temp_dir().join(format!("agent-modes-{}", std::process::id()));
        fs::create_dir_all(home.join(".agents/modes")).unwrap();
        fs::write(cache_path(&home, "ponytail"), "---\nname: ponytail\n---\nBe lazy.").unwrap();
        fs::write(cache_path(&home, "ultracave"), "---\nname: ultracave\n---\nFew word.").unwrap();
        let prompt = prompt(&home, "full", "ultra", "default");
        let styles = OUTPUT_STYLES.map(|style| super::prompt(&home, "off", "off", style));
        fs::remove_dir_all(&home).unwrap();
        assert_eq!(styles[0], "");
        for (style, prompt) in OUTPUT_STYLES.iter().zip(&styles).skip(1) {
            assert!(prompt.contains(&format!("\n# {style} Style Active\n")), "{style}");
        }
        assert_eq!(prompt, "<ponytail>\nPONYTAIL MODE ACTIVE — level: full\n\nBe lazy.\n</ponytail>\n\n<caveman>\nCAVEMAN MODE ACTIVE — mode: ultracave\n\nFew word.\n</caveman>");
    }

    #[test]
    fn stop_phrases_turn_off_their_mode() {
        assert_eq!(stopped_by("Stop caveman!"), (false, true));
        assert_eq!(stopped_by("normal mode"), (true, true));
        assert_eq!(stopped_by("stop ponytail from rewriting this"), (false, false));
    }
}
