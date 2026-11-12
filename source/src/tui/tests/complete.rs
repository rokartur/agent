use super::*;

#[test]
fn history_lists_a_repeated_prompt_once() {
    let entry = |prompt: &str| history::Entry { prompt: prompt.into(), cwd: "/repo".into(), session: "s1".into() };
    let entries = [entry("commits push pr merge"), entry("commits i push"), entry("commits push pr merge")];
    assert_eq!(matching_prompts(&entries, "commits", |_| true), ["commits push pr merge", "commits i push"]);
    assert_eq!(matching_prompts(&entries, "", |_| true), ["commits push pr merge", "commits i push"]);
}

#[test]
fn slash_menu_lists_prefix_matches_first_and_closes_after_a_space() {
    let commands: Vec<(String, String)> = ["compact", "skill:code-review", "copy"].iter().map(|n| (n.to_string(), String::new())).collect();
    let names = |text: &str| matching_commands(&commands, text).into_iter().map(|c| c.0).collect::<Vec<_>>();
    assert_eq!(names("/co"), ["compact", "copy", "skill:code-review"]);
    assert_eq!(names("/"), ["compact", "skill:code-review", "copy"]);
    assert!(names("/copy now").is_empty());
    assert!(names("copy").is_empty());
}

#[test]
fn slash_menu_completes_a_slash_typed_in_front_of_existing_text() {
    assert_eq!(slash_query("/skfix the bug", "/sk"), "/sk");
    assert_eq!(slash_query("/thinking m", "/thinking m"), "/thinking m");
    assert_eq!(slash_query("fix the bug /sk", "fix the bug /sk"), "fix the bug /sk");
}

#[test]
fn a_slash_word_after_text_offers_skills_but_a_path_or_shell_line_does_not() {
    assert_eq!(mid_text_skill("fix the bug /co"), Some("co"));
    assert_eq!(mid_text_skill("fix the bug /"), Some(""));
    assert_eq!(mid_text_skill("/co"), None);
    assert_eq!(mid_text_skill("look at /tmp/x"), None);
    assert_eq!(mid_text_skill("!ls /tm"), None);
    assert_eq!(mid_text_skill("fix /co the bug"), None);
}

#[test]
fn a_leading_slash_skill_runs_as_skill_and_a_command_name_stays_a_command() {
    let is_skill = |name: &str| name == "code-review";
    assert_eq!(as_skill_command("/code-review src/".into(), is_skill), "/skill:code-review src/");
    assert_eq!(as_skill_command("/code-review".into(), is_skill), "/skill:code-review");
    assert_eq!(as_skill_command("/compact".into(), is_skill), "/compact");
    assert_eq!(as_skill_command("fix /code-review".into(), is_skill), "fix /code-review");
}

#[test]
fn slash_menu_lists_known_arguments_after_the_name() {
    let names = |text: &str| matching_commands(&[], text).into_iter().map(|c| c.0).collect::<Vec<_>>();
    assert_eq!(names("/thinking m"), ["thinking minimal", "thinking medium", "thinking max"]);
    assert_eq!(names("/permissions "), ["permissions ask", "permissions auto", "permissions full access"]);
    assert_eq!(names("/plan e"), ["plan export", "plan exit"]);
    assert_eq!(names("/workspace r"), ["workspace remove"]);
    assert!(names("/thinking medium ").is_empty());
}

#[test]
fn at_menu_lists_a_directory_then_nested_names_with_folders_first_and_dotfiles_without_the_dot() {
    let root = std::env::temp_dir().join(format!("agent-at-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src/tui")).unwrap();
    std::fs::create_dir_all(root.join(".git")).unwrap();
    for file in ["b.md", ".env", "src/main.rs", "src/.envrc"] {
        std::fs::write(root.join(file), "").unwrap();
    }
    let paths = |query: &str| matching_paths(&root, query).into_iter().map(|p| p.0).collect::<Vec<_>>();
    assert_eq!(paths(""), ["src/", ".env", "b.md"]);
    assert_eq!(paths("."), [".git/", ".env", "src/.envrc"]);
    assert_eq!(paths("env"), [".env", "src/.envrc"]);
    assert_eq!(paths("src/e"), ["src/.envrc"]);
    assert_eq!(paths("src/"), ["src/tui/", "src/.envrc", "src/main.rs"]);
    assert_eq!(paths("src/M"), ["src/main.rs"]);
    assert_eq!(paths("tu"), ["src/tui/"]);
    assert_eq!(paths("m"), ["src/main.rs"]);
    assert!(paths("nope/").is_empty());
    std::fs::remove_dir_all(root).unwrap();
}
