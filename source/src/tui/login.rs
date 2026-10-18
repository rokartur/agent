use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::editor::Editor;
use super::panel;
use super::render::{BRIGHT, Line, MUTED, span, wrap};
use crate::api::accounts::{Account, Secret};
use crate::api::oauth::{self, Pending, Tokens};

const KINDS: [&str; 2] = ["Account (subscription)", "API key"];
const METHODS: [Method; 4] = [Method::ClaudeMax, Method::AnthropicKey, Method::OpenRouterKey, Method::Codex];

#[derive(Clone, Copy, PartialEq, Debug)]
enum Method {
    ClaudeMax,
    AnthropicKey,
    OpenRouterKey,
    Codex,
}

impl Method {
    fn label(self) -> &'static str {
        match self {
            Method::ClaudeMax => "Claude Pro/Max",
            Method::AnthropicKey => "Anthropic API key",
            Method::OpenRouterKey => "OpenRouter API key",
            Method::Codex => "ChatGPT Plus/Pro (Codex)",
        }
    }

    fn is_api_key(self) -> bool {
        matches!(self, Method::AnthropicKey | Method::OpenRouterKey)
    }
}

/// The /login panel: pick account or API key, search and pick a method, sign in or paste a key, then name the account.
pub enum Login {
    Kind {
        index: usize,
    },
    Choose {
        api: bool,
        query: Editor,
        index: usize,
    },
    /// Anthropic shows `code#state` after sign-in; the user pastes it here.
    ClaudeCode {
        pending: Pending,
        input: Editor,
    },
    ApiKey {
        provider: &'static str,
        title: &'static str,
        input: Editor,
    },
    /// A token exchange or the Codex browser callback is running; Esc sets `cancel`. `url` is the Codex sign-in page.
    Waiting {
        cancel: Arc<AtomicBool>,
        url: Option<String>,
    },
    Name {
        provider: &'static str,
        secret: Secret,
        input: Editor,
    },
}

type Exchange = Box<dyn FnOnce(&AtomicBool) -> Result<Tokens, String> + Send>;

/// What the app does after a key.
pub enum Action {
    None,
    Close,
    /// Copies the sign-in URL to the clipboard.
    Copy(String),
    /// Runs `exchange` off the UI thread with `cancel`, then hands its result to `finished`.
    Exchange {
        provider: &'static str,
        cancel: Arc<AtomicBool>,
        exchange: Exchange,
    },
    Save(Account),
}

impl Login {
    /// Stops a running exchange, so the Codex callback frees 127.0.0.1:1455 for the next /login.
    pub fn cancel(&self) {
        if let Login::Waiting { cancel, .. } = self {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Action {
        let code = key.code;
        if code == KeyCode::Char('y') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.url().map_or(Action::None, |url| Action::Copy(url.to_string()));
        }
        if code == KeyCode::Esc {
            if let Login::Choose { api, .. } = self {
                *self = Login::Kind { index: *api as usize };
                return Action::None;
            }
            self.cancel();
            return Action::Close;
        }
        match self {
            Login::Kind { index } => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(KINDS.len() - 1),
                KeyCode::Enter => *self = Login::Choose { api: *index == 1, query: Editor::new(), index: 0 },
                _ => {}
            },
            Login::Choose { api, query, index } => match code {
                KeyCode::Up => *index = index.saturating_sub(1),
                KeyCode::Down => *index = (*index + 1).min(methods(*api, query).len().saturating_sub(1)),
                KeyCode::Enter => return self.choose(),
                _ => {
                    if query.edit_key(key) {
                        *index = 0;
                    }
                }
            },
            Login::Waiting { .. } => {}
            Login::ClaudeCode { input, .. } | Login::ApiKey { input, .. } | Login::Name { input, .. } => match code {
                KeyCode::Enter if !input.text().trim().is_empty() => return self.submit(),
                _ => _ = input.edit_key(key),
            },
        }
        Action::None
    }

    pub fn paste(&mut self, text: &str) {
        if let Login::Choose { query, index, .. } = self {
            query.insert(text.trim());
            *index = 0;
        } else if let Login::ClaudeCode { input, .. } | Login::ApiKey { input, .. } | Login::Name { input, .. } = self {
            input.insert(text.trim());
        }
    }

    /// An exchange ended. Tokens move on to naming the account; after Esc, or from an older /login, the result is dropped.
    pub fn finished(&mut self, provider: &'static str, exchange_cancel: &Arc<AtomicBool>, result: Result<Tokens, String>) -> Result<(), String> {
        if !matches!(self, Login::Waiting { cancel, .. } if Arc::ptr_eq(cancel, exchange_cancel)) {
            return Ok(());
        }
        let name = if provider == "anthropic" { "claude" } else { "codex" };
        *self = Login::Name { provider, secret: Secret::OAuth(result?), input: Editor::from_text(name) };
        Ok(())
    }

    fn choose(&mut self) -> Action {
        let Login::Choose { api, query, index } = self else { return Action::None };
        let Some(&method) = methods(*api, query).get(*index) else { return Action::None };
        match method {
            Method::ClaudeMax => {
                let pending = oauth::start_claude();
                let url = pending.url.clone();
                oauth::open_browser(&url);
                *self = Login::ClaudeCode { pending, input: Editor::new() };
                Action::None
            }
            Method::AnthropicKey => {
                *self = Login::ApiKey { provider: "anthropic", title: method.label(), input: Editor::new() };
                Action::None
            }
            Method::OpenRouterKey => {
                *self = Login::ApiKey { provider: "openrouter", title: method.label(), input: Editor::new() };
                Action::None
            }
            Method::Codex => {
                let pending = oauth::start_codex();
                let url = pending.url.clone();
                oauth::open_browser(&url);
                let cancel = Arc::new(AtomicBool::new(false));
                *self = Login::Waiting { cancel: cancel.clone(), url: Some(url) };
                Action::Exchange { provider: "openai-codex", cancel, exchange: Box::new(move |cancel| oauth::finish_codex(&pending, cancel)) }
            }
        }
    }

    fn submit(&mut self) -> Action {
        let cancel = Arc::new(AtomicBool::new(false));
        match std::mem::replace(self, Login::Waiting { cancel: cancel.clone(), url: None }) {
            Login::ClaudeCode { pending, input } => {
                let code = input.text();
                Action::Exchange { provider: "anthropic", cancel, exchange: Box::new(move |_| oauth::finish_claude(&pending, &code)) }
            }
            Login::ApiKey { provider, input, .. } => {
                let name = if provider == "anthropic" { "api" } else { provider };
                *self = Login::Name { provider, secret: Secret::ApiKey(input.text().trim().into()), input: Editor::from_text(name) };
                Action::None
            }
            Login::Name { provider, secret, input } => Action::Save(Account { provider: provider.into(), name: input.text().trim().into(), secret }),
            other => {
                *self = other;
                Action::None
            }
        }
    }

    pub fn render(&self, width: usize, max_rows: usize) -> Vec<Line> {
        let (title, rows, hint): (std::borrow::Cow<str>, _, _) = match self {
            Login::Kind { .. } => {
                let rows = KINDS.iter().map(|c| panel::Row { left: c.to_string(), right: Vec::new() }).collect();
                ("Login: add an account or an API key?".into(), rows, "↑↓ navigate     enter choose     esc close")
            }
            Login::Choose { api, query, .. } => {
                let rows = methods(*api, query).iter().map(|method| panel::Row { left: method.label().to_string(), right: Vec::new() }).collect();
                (KINDS[*api as usize].into(), rows, "type to search     ↑↓ navigate     enter choose     esc back")
            }
            Login::ClaudeCode { input, .. } => {
                ("Claude login".into(), vec![field("Code", input.field(Some('•')))], "paste the code#state shown after sign-in     enter continue     ctrl+y copy link     esc cancel")
            }
            Login::ApiKey { title, input, .. } => ((*title).into(), vec![field("Key", input.field(Some('•')))], "paste the key     enter continue     esc cancel"),
            Login::Waiting { url, .. } => (
                "Login".into(),
                vec![panel::Row { left: "Waiting for the sign-in to finish...".into(), right: Vec::new() }],
                if url.is_some() { "ctrl+y copy link     esc cancel" } else { "esc cancel" },
            ),
            Login::Name { input, .. } => ("Account name".into(), vec![field("Name", input.field(None))], "enter save     esc cancel"),
        };
        let selected = if let Login::Kind { index } | Login::Choose { index, .. } = self { *index } else { 0 };
        let search = if let Login::Choose { query, .. } = self { Some(query.field(None)) } else { None };
        let view = panel::Panel { title: title.into(), tabs: &[], tab: 0, search, rows: &rows, selected, empty: "No matching login methods.", hint };
        let mut lines = view.render(width, max_rows);
        if let Some(url) = self.url() {
            // The whole link, wrapped, so it can be read or copied even when the browser did not open.
            let link = wrap(&[span("Sign in at: ", MUTED), span(url, BRIGHT.bold())], width, &Vec::new());
            let hint = lines.len().saturating_sub(1);
            lines.splice(hint..hint, link);
        }
        lines
    }

    /// The sign-in page of the current step, if it has one.
    fn url(&self) -> Option<&str> {
        match self {
            Login::ClaudeCode { pending, .. } => Some(&pending.url),
            Login::Waiting { url, .. } => url.as_deref(),
            _ => None,
        }
    }
}

/// Methods of the chosen kind whose label matches the search query, see `panel::search`.
fn methods(api: bool, query: &Editor) -> Vec<Method> {
    let kind = METHODS.into_iter().filter(|method| method.is_api_key() == api);
    panel::search(kind, query.text().trim(), |method| method.label().to_string())
}

fn field(label: &str, value: String) -> panel::Row {
    panel::Row { left: label.into(), right: vec![span(value, BRIGHT.bold())] }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_key_is_masked_then_named_and_saved() {
        let mut login = Login::Kind { index: 0 };
        login.key(KeyCode::Down.into());
        login.key(KeyCode::Enter.into());
        login.paste("ANTH");
        login.key(KeyCode::Enter.into());
        login.paste(" sk-ant-123 ");
        let shown: String = login.render(80, 8).iter().flatten().map(|s| s.text.as_str()).collect();
        assert!(!shown.contains("sk-ant") && shown.contains(&format!("{}\u{E0001}", "•".repeat(10))));
        login.key(KeyCode::Enter.into());
        login.key(KeyCode::Backspace.into());
        login.key(KeyCode::Char('2').into());
        let Action::Save(account) = login.key(KeyCode::Enter.into()) else { panic!("expected a save") };
        assert_eq!((account.provider.as_str(), account.name.as_str()), ("anthropic", "ap2"));
        assert!(matches!(account.secret, Secret::ApiKey(key) if key == "sk-ant-123"));
    }

    #[test]
    fn openrouter_key_is_saved_as_an_openrouter_account() {
        let mut login = Login::Kind { index: 1 };
        login.key(KeyCode::Enter.into());
        login.paste("openrouter");
        login.key(KeyCode::Enter.into());
        login.paste("sk-or-1");
        login.key(KeyCode::Enter.into());
        let Action::Save(account) = login.key(KeyCode::Enter.into()) else { panic!("expected a save") };
        assert_eq!((account.provider.as_str(), account.name.as_str()), ("openrouter", "openrouter"));
    }

    #[test]
    fn account_search_filters_and_esc_goes_back() {
        let mut login = Login::Kind { index: 0 };
        login.key(KeyCode::Enter.into());
        let Login::Choose { api, query, .. } = &login else { panic!("expected the method list") };
        assert_eq!(methods(*api, query), [Method::ClaudeMax, Method::Codex]);
        login.paste("codex");
        let Login::Choose { api, query, .. } = &login else { panic!("expected the method list") };
        assert_eq!(methods(*api, query), [Method::Codex]);
        login.key(KeyCode::Esc.into());
        assert!(matches!(login, Login::Kind { index: 0 }));
    }

    #[test]
    fn sign_in_link_is_shown_whole_in_the_panel_and_copied_with_ctrl_y() {
        let url = format!("https://auth.example/{}", "x".repeat(150));
        let mut login = Login::Waiting { cancel: Arc::default(), url: Some(url.clone()) };
        let shown: String = login.render(60, 8).iter().flatten().map(|s| s.text.as_str()).collect();
        assert!(shown.contains(&url));
        let ctrl_y = KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert!(matches!(login.key(ctrl_y), Action::Copy(copied) if copied == url));
    }

    #[test]
    fn an_older_exchange_finishing_leaves_the_new_login_waiting() {
        let current = Arc::default();
        let mut login = Login::Waiting { cancel: Arc::clone(&current), url: None };
        assert_eq!(login.finished("openai-codex", &Arc::default(), Err("cancelled".into())), Ok(()));
        assert!(matches!(login, Login::Waiting { .. }));
        assert_eq!(login.finished("openai-codex", &current, Err("denied".into())), Err("denied".into()));
    }
}
