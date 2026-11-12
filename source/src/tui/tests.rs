use super::*;
use serde_json::json;

mod app;
mod complete;
mod panels;
mod transcript;
mod view;

fn test_app(home: &Path) -> App {
    let cwd = home.join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    let (client, _) = crate::api::images::tests::fake_codex_client(home, &[]);
    let settings = crate::settings::load(home).unwrap();
    let session = Session::start(&home.join("sessions"), false, &cwd, "m").unwrap();
    let agent = Agent::new(client, &settings, home.to_path_buf(), cwd, session, home.join("sessions"), None);
    let setup = Setup { home: home.to_path_buf(), settings, prompt: None, pick_session: false, modes_error: None, plugin_updates: Vec::new(), missing_cwd: None, placed: None };
    App::new(&agent, setup, mpsc::channel().0, mpsc::channel().0)
}

fn text(line: &Line) -> String {
    line.iter().map(|s| s.text.as_str()).collect()
}
