use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

const DEFAULT_PREAMBLE: &str = "You are an expert coding assistant. You help users by reading files, executing commands, editing code, and writing new files.

<tools>
- Read: Read a file as anchored rows
- ReplaceLines, Edit, InsertLines: Edit a file by the anchors Read returned; Edit also by `file_path` and unique fragments, several in one call via `edits`
- Write: Create a file, or overwrite one whole
- Bash: Run shell commands
- Grep: Search file contents
</tools>

<rules>
- Edit a file by anchor or by Edit with `file_path` instead of rewriting it
- Use Grep, not grep through Bash
- Be concise in your responses
- Show file paths clearly when working with files
</rules>";
const CONTEXT_FILES: [&str; 3] = ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD"];

/// Preamble, addendum, project context, skills, cwd. ponytail and caveman come from `modes`.
/// The project's `.agents/` SYSTEM.md wins over the agent dir's (`~/.agents`); skills follow `prefer_global`.
/// An untrusted project gives only its AGENTS.md files.
pub fn system_prompt(agent_dir: &Path, cwd: &Path, trusted: bool, disabled_skills: &[String], prefer_global: bool) -> String {
    let mut sections = vec![override_file(agent_dir, cwd, trusted, "SYSTEM.md").unwrap_or_else(|| DEFAULT_PREAMBLE.into())];
    if let Some(addendum) = override_file(agent_dir, cwd, trusted, "APPEND_SYSTEM.md") {
        sections.push(tag("addendum", &addendum));
    }
    let context = project_context(agent_dir, cwd);
    if !context.is_empty() {
        sections.push(tag("project_context", &context));
    }
    // The host injects ponytail and caveman natively, so their skills would only duplicate the rules.
    let skills: Vec<Skill> = load_skills(agent_dir, cwd, trusted, disabled_skills, prefer_global).into_iter().filter(|s| s.model_invocable && s.name != "ponytail" && s.name != "caveman").collect();
    if !skills.is_empty() {
        sections.push(tag("skills", &format_skills(skills)));
    }
    sections.retain(|s| !s.is_empty());
    sections.join("\n\n")
}

/// Sent after the cached system prompt, so moving to another directory or adding one keeps that cache.
pub fn cwd_section(cwd: &Path, extra_dirs: &[PathBuf]) -> String {
    let mut content = cwd.display().to_string();
    if !extra_dirs.is_empty() {
        let dirs: Vec<String> = extra_dirs.iter().map(|dir| dir.display().to_string()).collect();
        content.push_str(&format!("\nAdditional directories: {}", dirs.join(", ")));
    }
    tag("cwd", &content)
}

fn tag(name: &str, content: &str) -> String {
    format!("<{name}>\n{content}\n</{name}>")
}

pub fn read_text(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(text.strip_prefix('\u{feff}').map(String::from).unwrap_or(text))
}

/// `.agents` in cwd and its parents up to the git root, nearest first; none for an untrusted project.
pub fn project_agents_dirs(cwd: &Path, trusted: bool) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if !trusted {
        return dirs;
    }
    for dir in cwd.ancestors() {
        dirs.push(dir.join(".agents"));
        if dir.join(".git").exists() {
            break;
        }
    }
    dirs
}

/// The directory /trust records: the git root above `cwd`, or `cwd` outside a repo.
pub fn project_root(cwd: &Path) -> PathBuf {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(&cwd).to_path_buf()
}

pub fn is_trusted(trusted_projects: &[String], dir: &Path) -> bool {
    trusted_projects.contains(&project_root(dir).display().to_string())
}

/// What an untrusted project would add from its `.agents` dirs, listed in the trust question.
pub fn untrusted_files(agent_dir: &Path, cwd: &Path) -> Vec<PathBuf> {
    let names = ["SYSTEM.md", "APPEND_SYSTEM.md", "skills", "prompts", "swarm-prompt.md", "edit-hooks.json", "workflows"];
    let dirs = project_agents_dirs(cwd, true).into_iter().filter(|dir| dir != agent_dir);
    dirs.flat_map(|dir| names.map(|name| dir.join(name))).filter(|path| path.exists()).collect()
}

fn override_file(agent_dir: &Path, cwd: &Path, trusted: bool, name: &str) -> Option<String> {
    project_agents_dirs(cwd, trusted).iter().chain([&agent_dir.to_path_buf()]).find_map(|dir| read_text(&dir.join(name)))
}

fn context_file(dir: &Path) -> Option<(PathBuf, String)> {
    CONTEXT_FILES.iter().map(|name| dir.join(name)).find_map(|path| read_text(&path).map(|text| (path, text)))
}

fn instructions_block(path: &Path, content: &str) -> String {
    format!("<project_instructions path=\"{}\">\n{content}\n</project_instructions>", path.display())
}

/// AGENTS.md files on the way to `file`: below `cwd`, or from an extra dir down, skipping those in `loaded`.
/// The agents.md convention: the file closest to the code being touched applies to it.
pub fn nested_context(cwd: &Path, extra_dirs: &[PathBuf], file: &Path, loaded: &mut HashSet<PathBuf>) -> String {
    let Some(parent) = file.parent() else { return String::new() };
    let below_cwd = parent.strip_prefix(cwd).ok().filter(|relative| relative.components().all(|component| matches!(component, Component::Normal(_))));
    let mut dirs = Vec::new();
    let mut dir = cwd.to_path_buf();
    if let Some(relative) = below_cwd {
        for name in relative.components() {
            dir.push(name);
            dirs.push(dir.clone());
        }
    } else {
        // Extra dirs are canonical, so `../api/x.rs` and symlinks must resolve before they match.
        let Ok(parent) = parent.canonicalize() else { return String::new() };
        let Some(extra) = extra_dirs.iter().find(|extra| parent.starts_with(extra)) else { return String::new() };
        // cwd's own AGENTS.md is in the system prompt, an extra dir's is not.
        dirs.extend(parent.ancestors().take_while(|ancestor| ancestor.starts_with(extra)).map(Path::to_path_buf));
        dirs.reverse();
    }
    let mut blocks = Vec::new();
    for dir in dirs {
        if let Some((path, content)) = context_file(&dir)
            && loaded.insert(path.clone())
        {
            blocks.push(instructions_block(&path, &content));
        }
    }
    blocks.join("\n\n")
}

fn project_context(agent_dir: &Path, cwd: &Path) -> String {
    let mut files: Vec<(PathBuf, String)> = context_file(agent_dir).into_iter().collect();
    let mut ancestors: Vec<(PathBuf, String)> = cwd.ancestors().filter_map(context_file).collect();
    ancestors.reverse();
    for file in ancestors {
        if !files.iter().any(|(path, _)| *path == file.0) {
            files.push(file);
        }
    }
    if files.is_empty() {
        return String::new();
    }
    let mut parts = vec!["Project-specific instructions and guidelines:".to_string()];
    for (path, content) in files {
        parts.push(instructions_block(&path, &content));
    }
    parts.join("\n\n")
}

pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// False for `disable-model-invocation: true`: listed for `/skill:name`, left out of the system prompt.
    pub model_invocable: bool,
    /// The same-named skill on the other side (project or global) this one won over.
    pub shadows: Option<PathBuf>,
}

/// `.agents` dirs in the order they win a name conflict, each flagged global: the preferred side first, project ones nearest first.
fn agents_dirs(agent_dir: &Path, cwd: &Path, trusted: bool, prefer_global: bool) -> Vec<(PathBuf, bool)> {
    let global = std::iter::once((agent_dir.to_path_buf(), true));
    // Outside a git repo the walk up reaches ~/.agents itself.
    let project = project_agents_dirs(cwd, trusted).into_iter().filter(|dir| dir != agent_dir).map(|dir| (dir, false));
    if prefer_global { global.chain(project).collect() } else { project.chain(global).collect() }
}

/// Skills from `~/.agents/skills` and `.agents/skills` in cwd and its parents up to the git root.
pub fn load_skills(agent_dir: &Path, cwd: &Path, trusted: bool, disabled: &[String], prefer_global: bool) -> Vec<Skill> {
    let mut found: Vec<(Skill, bool)> = Vec::new();
    let mut seen = HashSet::new();
    for (dir, global) in agents_dirs(agent_dir, cwd, trusted, prefer_global) {
        let mut files = Vec::new();
        collect_skill_files(&dir.join("skills"), true, &mut files);
        for path in files {
            if !seen.insert(path.canonicalize().unwrap_or_else(|_| path.clone())) {
                continue;
            }
            let Some(skill) = parse_skill(&path) else { continue };
            if disabled.contains(&skill.name) {
                continue;
            }
            match found.iter_mut().find(|(winner, _)| winner.name == skill.name) {
                Some((winner, winner_global)) if *winner_global != global => _ = winner.shadows.get_or_insert(skill.path),
                Some(_) => {}
                None => found.push((skill, global)),
            }
        }
    }
    found.into_iter().map(|(skill, _)| skill).collect()
}

fn collect_skill_files(dir: &Path, root: bool, out: &mut Vec<PathBuf>) {
    let skill = dir.join("SKILL.md");
    if skill.is_file() {
        out.push(skill);
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            collect_skill_files(&path, false, out);
        } else if root && name.ends_with(".md") {
            out.push(path);
        }
    }
}

pub fn parse_skill(path: &Path) -> Option<Skill> {
    let text = read_text(path)?;
    let fields = frontmatter(&text)?;
    let field = |key: &str| field(&fields, key).map(String::from);
    let model_invocable = field("disable-model-invocation").as_deref() != Some("true");
    let description = field("description").filter(|d| !d.trim().is_empty())?;
    let dir_name = path.parent()?.file_name()?.to_string_lossy().into_owned();
    let name = field("name").filter(|n| !n.is_empty()).unwrap_or(dir_name);
    Some(Skill { name, description, path: path.to_path_buf(), model_invocable, shadows: None })
}

/// pi's `/skill:name args` expansion: the skill body wrapped in a `<skill>` block, then the args.
pub fn expand_skill(text: &str, skills: &[Skill]) -> Result<String, String> {
    let Some(rest) = text.strip_prefix("/skill:") else { return Ok(text.to_string()) };
    let (name, args) = rest.split_once(' ').map_or((rest, ""), |(name, args)| (name, args.trim()));
    let Some(skill) = skills.iter().find(|s| s.name == name) else { return Ok(text.to_string()) };
    let content = read_text(&skill.path).ok_or(format!("Cannot read {}", skill.path.display()))?;
    let body = without_frontmatter(&content);
    let base = skill.path.parent().unwrap_or(&skill.path).display();
    let block = format!("<skill name=\"{}\" location=\"{}\">\nReferences are relative to {base}.\n\n{body}\n</skill>", skill.name, skill.path.display());
    Ok(if args.is_empty() { block } else { format!("{block}\n\n{args}") })
}

/// A prompt template, run as `/prompt:name args`.
pub struct Template {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    /// The same-named template on the other side (project or global) this one won over.
    pub shadows: Option<PathBuf>,
}

/// `*.md` in `~/.agents/prompts` and the project's `.agents/prompts` up to the git root, sorted by name.
pub fn load_templates(agent_dir: &Path, cwd: &Path, trusted: bool, prefer_global: bool) -> Vec<Template> {
    let mut found: Vec<(Template, bool)> = Vec::new();
    for (dir, global) in agents_dirs(agent_dir, cwd, trusted, prefer_global) {
        let Ok(entries) = std::fs::read_dir(dir.join("prompts")) else { continue };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|ext| ext == "md")).collect();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_stem().and_then(|stem| stem.to_str()).map(str::to_string) else { continue };
            match found.iter_mut().find(|(winner, _)| winner.name == name) {
                Some((winner, winner_global)) if *winner_global != global => _ = winner.shadows.get_or_insert(path),
                Some(_) => {}
                None => {
                    let fields = read_text(&path).and_then(|text| frontmatter(&text)).unwrap_or_default();
                    let description = field(&fields, "description").unwrap_or_default().to_string();
                    found.push((Template { name, description, path, shadows: None }, global));
                }
            }
        }
    }
    let mut templates: Vec<Template> = found.into_iter().map(|(template, _)| template).collect();
    templates.sort_by(|a, b| a.name.cmp(&b.name));
    templates
}

/// The template body with its placeholders filled from `args`, split like a shell line.
pub fn expand_template(template: &Template, args: &str) -> Result<String, String> {
    let content = read_text(&template.path).ok_or(format!("Cannot read {}", template.path.display()))?;
    Ok(substitute_args(without_frontmatter(&content), &split_args(args)))
}

/// `/plan args`: a loaded `plan` template overrides the built-in prompt.
pub fn expand_plan(templates: &[Template], args: &str) -> Result<String, String> {
    match templates.iter().find(|t| t.name == "plan") {
        Some(template) => expand_template(template, args),
        None => Ok(substitute_args(include_str!("plan-prompt.md"), &split_args(args))),
    }
}

/// Splits on whitespace; `"..."` and `'...'` keep their spaces and lose the quotes, like pi.
fn split_args(text: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    for c in text.chars() {
        match quote {
            Some(open) if c == open => quote = None,
            Some(_) => current.push(c),
            None if c == '"' || c == '\'' => quote = Some(c),
            None if c.is_whitespace() => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            None => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// pi's placeholders: `$1`, `$@` and `$ARGUMENTS`, `${1:-default}`, `${@:-default}`, `${@:2}` and `${@:2:3}`.
/// Values are not substituted again, and a `$` that starts no placeholder stays as is.
fn substitute_args(content: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let mut out = String::new();
    let mut rest = content;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        rest = &rest[at + 1..];
        match placeholder(rest, args, &all) {
            Some((value, consumed)) => {
                out.push_str(&value);
                rest = &rest[consumed..];
            }
            None => out.push('$'),
        }
    }
    out.push_str(rest);
    out
}

/// The value of the placeholder at the start of `after` (the text past a `$`) and how many bytes it spans.
fn placeholder(after: &str, args: &[String], all: &str) -> Option<(String, usize)> {
    let is_number = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    // Callers check `is_number` first, so only an overflowing index lands here, and it is past every argument.
    let number = |text: &str| text.parse::<usize>().unwrap_or(usize::MAX);
    let positional = |n: usize| n.checked_sub(1).and_then(|i| args.get(i)).cloned().unwrap_or_default();
    if let Some(braced) = after.strip_prefix('{') {
        let (inner, _) = braced.split_once('}')?;
        let consumed = inner.len() + 2;
        if let Some((target, default)) = inner.split_once(":-") {
            let value = match target {
                "@" | "ARGUMENTS" => all.to_string(),
                n if is_number(n) => positional(number(n)),
                _ => return None,
            };
            return Some((if value.is_empty() { default.to_string() } else { value }, consumed));
        }
        let slice = inner.strip_prefix("@:")?;
        let (start, length) = slice.split_once(':').map_or((slice, None), |(start, length)| (start, Some(length)));
        if !is_number(start) || length.is_some_and(|l| !is_number(l)) {
            return None;
        }
        let picked = args.iter().skip(number(start).saturating_sub(1)).take(length.map_or(usize::MAX, number));
        return Some((picked.map(String::as_str).collect::<Vec<_>>().join(" "), consumed));
    }
    if after.starts_with("ARGUMENTS") {
        return Some((all.to_string(), "ARGUMENTS".len()));
    }
    if after.starts_with('@') {
        return Some((all.to_string(), 1));
    }
    let digits = after.bytes().take_while(u8::is_ascii_digit).count();
    (digits > 0).then(|| (positional(number(&after[..digits])), digits))
}

fn without_frontmatter(content: &str) -> &str {
    content.strip_prefix("---").and_then(|rest| rest.split_once("\n---")).map_or(content, |(_, body)| body).trim()
}

fn field<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == key).map(|(_, value)| value.as_str())
}

/// Top-level `key: value` pairs of a YAML frontmatter; enough YAML for skill files.
fn frontmatter(text: &str) -> Option<Vec<(String, String)>> {
    let (first, body) = text.split_once('\n')?;
    if first.trim_end() != "---" {
        return None;
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    for line in body.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim_end() == "---" {
            return Some(fields);
        }
        if line.starts_with([' ', '\t']) || line.is_empty() {
            if let Some((_, value)) = fields.last_mut() {
                if !value.is_empty() {
                    value.push(' ');
                }
                value.push_str(line.trim());
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else { continue };
        let value = value.trim();
        let value = if value.starts_with(['>', '|']) { "" } else { value };
        let value = match value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
            Some(quoted) => quoted.replace("\\\"", "\"").replace("\\\\", "\\"),
            None => value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')).map_or(value.to_string(), |quoted| quoted.replace("''", "'")),
        };
        fields.push((key.trim().to_string(), value));
    }
    None
}

/// pi-cache-optimizer's compressed skill list: one header per skills root, one line per skill.
fn format_skills(skills: Vec<Skill>) -> String {
    let mut by_root: BTreeMap<PathBuf, Vec<(String, String)>> = BTreeMap::new();
    let mut explicit: Vec<(PathBuf, String, String)> = Vec::new();
    for skill in skills {
        let description = skill.description.split_whitespace().collect::<Vec<_>>().join(" ");
        let dir = skill.path.parent().expect("a skill file sits in a directory");
        let conventional = skill.path.file_name().is_some_and(|f| f == "SKILL.md") && dir.file_name().is_some_and(|d| *d == *skill.name);
        if !conventional {
            explicit.push((skill.path, skill.name, description));
            continue;
        }
        let root = dir.parent().expect("a skill directory sits in a skills root").to_path_buf();
        by_root.entry(root).or_default().push((skill.name, description));
    }
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".to_string(),
        "Load a skill with the Skill tool when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
    ];
    for (root, mut entries) in by_root {
        let root = root.display();
        lines.extend([String::new(), format!("## Skills in {root}/"), format!("Each skill file is at {root}/<name>/SKILL.md"), String::new()]);
        entries.sort();
        for (name, description) in entries {
            lines.push(format!("- {name}: {description}"));
        }
    }
    if !explicit.is_empty() {
        lines.extend([String::new(), "## Skills with explicit file paths".to_string(), String::new()]);
        explicit.sort();
        for (path, name, description) in explicit {
            lines.push(format!("- {name} (file: {}): {description}", path.display()));
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_reads_plain_quoted_and_folded_values() {
        let text = "--- \nname: \"say \\\"hi\\\"\"\ndescription: >\n  first line\n  second line\nlicense: MIT\n---\nbody";
        let fields = frontmatter(text).unwrap();
        assert_eq!(fields[0], ("name".into(), "say \"hi\"".into()));
        assert_eq!(fields[1], ("description".into(), "first line second line".into()));
        assert_eq!(fields[2], ("license".into(), "MIT".into()));
    }

    #[test]
    fn system_prompt_is_the_same_in_every_directory_so_it_stays_cached() {
        let root = std::env::temp_dir().join(format!("agent-stable-{}", std::process::id()));
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        assert_eq!(system_prompt(&root, &root.join("a"), true, &[], false), system_prompt(&root, &root.join("b"), true, &[], false));
        assert_eq!(cwd_section(&root.join("a"), &[]), format!("<cwd>\n{}\n</cwd>", root.join("a").display()));
        let extra = [root.join("b"), root.join("c")];
        assert_eq!(cwd_section(&root.join("a"), &extra), format!("<cwd>\n{}\nAdditional directories: {}, {}\n</cwd>", root.join("a").display(), extra[0].display(), extra[1].display()));
    }

    #[test]
    fn nested_agents_md_is_attached_once_and_only_below_cwd() {
        let cwd = std::env::temp_dir().join(format!("agent-nested-{}", std::process::id()));
        std::fs::create_dir_all(cwd.join("sub/deep")).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "root").unwrap();
        std::fs::write(cwd.join("sub/AGENTS.md"), "sub rules").unwrap();
        let mut loaded = HashSet::new();
        let file = cwd.join("sub/deep/x.rs");
        let first = nested_context(&cwd, &[], &file, &mut loaded);
        let second = nested_context(&cwd, &[], &file, &mut loaded);
        std::fs::remove_dir_all(&cwd).unwrap();
        assert_eq!(first, format!("<project_instructions path=\"{}\">\nsub rules\n</project_instructions>", cwd.join("sub/AGENTS.md").display()));
        assert_eq!(second, "");
        assert_eq!(nested_context(&cwd, &[], &cwd.join("x.rs"), &mut loaded), "");
    }

    #[test]
    fn extra_dir_agents_md_is_attached_from_the_extra_dir_down() {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!("agent-nested-extra-{}", std::process::id()));
        let extra = root.join("api");
        std::fs::create_dir_all(extra.join("sub")).unwrap();
        std::fs::create_dir_all(root.join("repo")).unwrap();
        std::fs::write(extra.join("AGENTS.md"), "api rules").unwrap();
        std::fs::write(extra.join("sub/AGENTS.md"), "sub rules").unwrap();
        let file = extra.join("sub/x.rs");
        let outside = nested_context(&root.join("repo"), &[], &file, &mut HashSet::new());
        let attached = nested_context(&root.join("repo"), std::slice::from_ref(&extra), &file, &mut HashSet::new());
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(outside, "");
        let block = |path: PathBuf, text: &str| format!("<project_instructions path=\"{}\">\n{text}\n</project_instructions>", path.display());
        assert_eq!(attached, format!("{}\n\n{}", block(extra.join("AGENTS.md"), "api rules"), block(extra.join("sub/AGENTS.md"), "sub rules")));
    }

    #[test]
    fn project_template_replaces_a_global_one_and_takes_the_arguments() {
        let root = std::env::temp_dir().join(format!("agent-templates-{}", std::process::id()));
        std::fs::create_dir_all(root.join("home/prompts")).unwrap();
        std::fs::create_dir_all(root.join("repo/.agents/prompts")).unwrap();
        std::fs::write(root.join("home/prompts/review.md"), "global").unwrap();
        std::fs::write(root.join("home/prompts/commit.md"), "---\ndescription: Commit\n---\nCommit. Context: $ARGUMENTS").unwrap();
        std::fs::write(root.join("repo/.agents/prompts/review.md"), "Review $ARGUMENTS").unwrap();
        let templates = load_templates(&root.join("home"), &root.join("repo"), true, false);
        let expanded: Vec<(String, String, String)> = templates.iter().map(|t| (t.name.clone(), t.description.clone(), expand_template(t, "HEAD~2").unwrap())).collect();
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(expanded, [("commit".into(), "Commit".into(), "Commit. Context: HEAD~2".into()), ("review".into(), String::new(), "Review HEAD~2".into())]);
    }

    #[test]
    fn plan_uses_the_plan_template_when_one_is_loaded() {
        let root = std::env::temp_dir().join(format!("agent-plan-template-{}", std::process::id()));
        std::fs::create_dir_all(root.join("home/prompts")).unwrap();
        let builtin = expand_plan(&load_templates(&root.join("home"), &root, true, false), "add login");
        std::fs::write(root.join("home/prompts/plan.md"), "My plan for $ARGUMENTS").unwrap();
        let custom = expand_plan(&load_templates(&root.join("home"), &root, true, false), "add login");
        std::fs::remove_dir_all(&root).unwrap();
        assert!(builtin.unwrap().contains("Task: add login"));
        assert_eq!(custom.unwrap(), "My plan for add login");
    }

    #[test]
    fn project_agents_dir_is_found_from_a_subdirectory_up_to_the_git_root() {
        let root = std::env::temp_dir().join(format!("agent-project-dir-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".agents")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("repo/.agents/prompts")).unwrap();
        std::fs::create_dir_all(root.join("repo/src")).unwrap();
        std::fs::write(root.join(".agents/APPEND_SYSTEM.md"), "outside the repo").unwrap();
        std::fs::write(root.join("repo/.agents/SYSTEM.md"), "repo system").unwrap();
        std::fs::write(root.join("repo/.agents/prompts/review.md"), "Review").unwrap();
        let cwd = root.join("repo/src");
        let templates: Vec<String> = load_templates(&root.join("home"), &cwd, true, false).into_iter().map(|t| t.name).collect();
        let system = system_prompt(&root.join("home"), &cwd, true, &[], false);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(templates, ["review"]);
        assert!(system.starts_with("repo system") && !system.contains("outside the repo"), "{system}");
    }

    #[test]
    fn same_named_skills_and_prompts_follow_the_priority_and_name_what_they_shadow() {
        let root = std::env::temp_dir().join(format!("agent-conflicts-{}", std::process::id()));
        let (home, repo) = (root.join("home"), root.join("repo"));
        for dir in [home.join("skills/tdd"), home.join("prompts"), repo.join(".git"), repo.join(".agents/skills/tdd"), repo.join(".agents/prompts")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let (global_skill, project_skill) = (home.join("skills/tdd/SKILL.md"), repo.join(".agents/skills/tdd/SKILL.md"));
        let (global_prompt, project_prompt) = (home.join("prompts/review.md"), repo.join(".agents/prompts/review.md"));
        for path in [&global_skill, &project_skill, &global_prompt, &project_prompt] {
            std::fs::write(path, "---\ndescription: x\n---\n").unwrap();
        }
        let resolved = |prefer_global| {
            let skills = load_skills(&home, &repo, true, &[], prefer_global);
            let templates = load_templates(&home, &repo, true, prefer_global);
            let skill = skills.into_iter().map(|s| (s.path, s.shadows)).collect::<Vec<_>>();
            let template = templates.into_iter().map(|t| (t.path, t.shadows)).collect::<Vec<_>>();
            (skill, template)
        };
        let (project_first, global_first) = (resolved(false), resolved(true));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(project_first, (vec![(project_skill.clone(), Some(global_skill.clone()))], vec![(project_prompt.clone(), Some(global_prompt.clone()))]));
        assert_eq!(global_first, (vec![(global_skill, Some(project_skill))], vec![(global_prompt, Some(project_prompt))]));
    }

    #[test]
    fn an_untrusted_project_keeps_its_agents_md_and_loses_its_agents_dir() {
        let root = std::env::temp_dir().join(format!("agent-untrusted-{}", std::process::id()));
        let (home, repo) = (root.join("home"), root.join("repo"));
        for dir in [home.join("skills/global"), repo.join(".git"), repo.join(".agents/skills/project"), repo.join(".agents/prompts")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for path in [home.join("skills/global/SKILL.md"), repo.join(".agents/skills/project/SKILL.md")] {
            std::fs::write(path, "---\ndescription: x\n---\n").unwrap();
        }
        std::fs::write(repo.join(".agents/prompts/review.md"), "Review").unwrap();
        std::fs::write(repo.join(".agents/SYSTEM.md"), "repo system").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "repo rules").unwrap();
        let skills: Vec<String> = load_skills(&home, &repo, false, &[], false).into_iter().map(|s| s.name).collect();
        let templates = load_templates(&home, &repo, false, false);
        let system = system_prompt(&home, &repo, false, &[], false);
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(skills, ["global"]);
        assert!(templates.is_empty());
        assert!(!system.contains("repo system") && system.contains("repo rules"), "{system}");
    }

    #[test]
    fn template_placeholders_follow_pi() {
        let args = split_args(r#"src "two words" 'x y' last"#);
        assert_eq!(args, ["src", "two words", "x y", "last"]);
        let cases = [
            ("$1 and $2", "src and two words"),
            ("$@ | $ARGUMENTS", "src two words x y last | src two words x y last"),
            ("${2:-none} ${9:-none} ${@:-none}", "two words none src two words x y last"),
            ("${@:3} / ${@:2:2} / ${@:0:1}", "x y last / two words x y / src"),
            ("$9|$0|$ alone|${1}|$x", "||$ alone|${1}|$x"),
        ];
        for (template, expected) in cases {
            assert_eq!(substitute_args(template, &args), expected, "{template}");
        }
        assert_eq!(substitute_args("${@:-default}", &[]), "default");
        assert_eq!(substitute_args("$1", &["$2".into(), "no".into()]), "$2");
    }
}
