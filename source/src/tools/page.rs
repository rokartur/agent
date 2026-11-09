//! Page: HTML the model writes, served to the user's browser by the session server, which can answer back.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DOCUMENT_HEAD: &str =
    concat!("<style>\n", include_str!("page/document.css"), "</style>\n<script>\n", include_str!("page/bridge.js"), "</script>\n<script>\n", include_str!("page/charts.js"), "</script>");
const MOCKS_HEAD: &str = concat!("<style>\n", include_str!("page/mocks.css"), "</style>\n<script>\n", include_str!("page/bridge.js"), "</script>");
const MOCKS_JS: &str = include_str!("page/mocks.js");
/// The page reaches only its own server: data from the session cannot be sent to another host by a fetch or a load.
const CSP: &str =
    "default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; img-src data:; font-src data:; connect-src 'self'; form-action 'none'; base-uri 'none'; frame-ancestors 'none'";
const MAX_REQUEST_BYTES: u64 = 1 << 20;
const LETTERS: [&str; 5] = ["A", "B", "C", "D", "E"];

/// The pages one session process published and the listener that serves them, started by the first Page call.
pub struct Pages {
    /// `http://<ip>:<port>/<token>`; a page is `<base>/<number>`.
    base: String,
    server: Arc<Server>,
    submits: Receiver<(usize, String)>,
}

pub struct Published {
    /// The page's number in its URL, unique in this process; the file name restarts at 1 in every session.
    pub number: usize,
    pub url: String,
    pub path: PathBuf,
}

impl Pages {
    /// `owner` is the Tailscale user whose devices may load pages; without Tailscale the caller passes None and 127.0.0.1.
    pub fn start(ip: IpAddr, owner: Option<u64>) -> Result<Pages, String> {
        let listener = TcpListener::bind(SocketAddr::new(ip, 0)).map_err(|e| format!("Cannot listen for pages on {ip}: {e}"))?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let token = crate::api::oauth::random_hex(16);
        let (sender, submits) = mpsc::channel();
        let server = Arc::new(Server { prefix: format!("/{token}/"), published: Mutex::new(Vec::new()), waiting: Mutex::new(None), submits: sender, owner, checked: Mutex::new(HashMap::new()) });
        let accepting = server.clone();
        std::thread::spawn(move || accept(&listener, &accepting));
        Ok(Pages { base: format!("http://{address}/{token}"), server, submits })
    }

    /// Writes `html` as the next numbered file in `dir`, readable by this user only like the session file, and serves it.
    /// With `wait` the page may answer from the moment it is served, before `wait` is called.
    pub fn publish(&self, dir: &Path, html: &str, wait: bool) -> Result<Published, String> {
        let parent = dir.parent().expect("pages live in a session's folder");
        std::fs::create_dir_all(parent).map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
        crate::process::private_dir(dir)?;
        let mut file_number = std::fs::read_dir(dir).map_err(|e| format!("Cannot list {}: {e}", dir.display()))?.count() + 1;
        let (mut file, path) = loop {
            let path = dir.join(format!("{file_number}.html"));
            match crate::process::private_file().write(true).create_new(true).open(&path) {
                Ok(file) => break (file, path),
                // A deleted earlier page leaves the count below the highest number.
                Err(e) if e.kind() == ErrorKind::AlreadyExists => file_number += 1,
                Err(e) => return Err(format!("Cannot write {}: {e}", path.display())),
            }
        };
        file.write_all(html.as_bytes()).map_err(|e| format!("Cannot write {}: {e}", path.display()))?;
        let mut published = self.server.published.lock().unwrap();
        published.push(path.clone());
        let number = published.len();
        if wait {
            *self.server.waiting.lock().unwrap() = Some(number);
        }
        Ok(Published { number, url: format!("{}/{number}", self.base), path })
    }

    /// What page `number` passed to `agent.submit`; None once `abort` is set. Only the waited page may submit.
    pub fn wait(&self, number: usize, abort: &AtomicBool) -> Option<String> {
        *self.server.waiting.lock().unwrap() = Some(number);
        let answer = loop {
            if abort.load(Ordering::Relaxed) {
                break None;
            }
            match self.submits.recv_timeout(Duration::from_millis(200)) {
                Ok((from, body)) if from == number => break Some(body),
                // A submit that raced the end of an earlier wait carries that page's number and is dropped.
                Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break None,
            }
        };
        *self.server.waiting.lock().unwrap() = None;
        answer
    }
}

struct Server {
    /// `/<token>/`: a request without it gets the same 404 as a page that does not exist.
    prefix: String,
    /// Every published page; page `n` is at index `n - 1`.
    published: Mutex<Vec<PathBuf>>,
    waiting: Mutex<Option<usize>>,
    submits: Sender<(usize, String)>,
    owner: Option<u64>,
    /// Peers `tailscale whois` already judged, admitted or not, so a page view does not run it once per request.
    checked: Mutex<HashMap<IpAddr, bool>>,
}

fn accept(listener: &TcpListener, server: &Arc<Server>) {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let server = server.clone();
                std::thread::spawn(move || serve(stream, &server));
            }
            Err(error) => {
                eprintln!("Cannot accept a page connection: {error}");
                // EMFILE, ENFILE and ENOBUFS repeat at once; the pause keeps this loop from spinning on them.
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn serve(stream: TcpStream, server: &Server) {
    if let Some(owner) = server.owner {
        let Ok(peer) = stream.peer_addr() else { return };
        let known = server.checked.lock().unwrap().get(&peer.ip()).copied();
        let admitted = known.unwrap_or_else(|| {
            let check = crate::tailnet::same_owner(peer.ip(), owner);
            if let Err(refusal) = &check {
                eprintln!("Refused page requests from {}: {refusal}", peer.ip());
            }
            server.checked.lock().unwrap().insert(peer.ip(), check.is_ok());
            check.is_ok()
        });
        if !admitted {
            return;
        }
    }
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
    let reply = match read_request(&stream) {
        Ok((method, path, body)) => server.respond(&method, &path, body),
        Err(status) => response(status, b""),
    };
    let _ = (&stream).write_all(&reply);
}

impl Server {
    fn respond(&self, method: &str, path: &str, body: String) -> Vec<u8> {
        let Some(number) = path.strip_prefix(&self.prefix).and_then(|number| number.parse::<usize>().ok()) else {
            return response("404 Not Found", b"");
        };
        let Some(file) = number.checked_sub(1).and_then(|index| self.published.lock().unwrap().get(index).cloned()) else {
            return response("404 Not Found", b"");
        };
        match method {
            "GET" => match std::fs::read(&file) {
                Ok(html) => response("200 OK", &html),
                Err(_) => response("404 Not Found", b""),
            },
            "POST" if *self.waiting.lock().unwrap() == Some(number) => {
                let _ = self.submits.send((number, body));
                response("204 No Content", b"")
            }
            "POST" => response("409 Conflict", b""),
            _ => response("405 Method Not Allowed", b""),
        }
    }
}

/// The method, path and body of one HTTP/1.1 request, or the status that refuses it.
fn read_request(stream: &TcpStream) -> Result<(String, String, String), &'static str> {
    const BAD: &str = "400 Bad Request";
    let mut reader = BufReader::new(stream.take(MAX_REQUEST_BYTES));
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|_| BAD)?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().ok_or(BAD)?.to_string(), parts.next().ok_or(BAD)?.to_string());
    let mut length: u64 = 0;
    loop {
        line.clear();
        reader.read_line(&mut line).map_err(|_| BAD)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().map_err(|_| BAD)?;
        }
    }
    // Checked before the allocation: a failed allocation aborts the whole session, not this thread.
    if length > MAX_REQUEST_BYTES {
        return Err("413 Content Too Large");
    }
    // The headers used up part of the limit; the body gets its own, on top of what the reader already buffered.
    reader.get_mut().set_limit(length);
    let mut body = vec![0; length as usize];
    reader.read_exact(&mut body).map_err(|_| BAD)?;
    Ok((method, path, String::from_utf8_lossy(&body).into_owned()))
}

fn response(status: &str, body: &[u8]) -> Vec<u8> {
    // RFC 9110 section 8.6: a 204 carries no Content-Length.
    let length = if status.starts_with("204") { String::new() } else { format!("Content-Length: {}\r\n", body.len()) };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n{length}Content-Security-Policy: {CSP}\r\nX-DNS-Prefetch-Control: off\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n"
    );
    [head.as_bytes(), body].concat()
}

/// The whole HTML file for a Page call: a document around `html`, or a choice between `mocks`.
pub fn render(input: &Value) -> Result<String, String> {
    let title = escape(input["title"].as_str().ok_or("title required.")?);
    match (input["html"].as_str(), input["mocks"].as_array()) {
        (Some(html), None) => Ok(shell(&title, DOCUMENT_HEAD, &format!("<main>\n<h1>{title}</h1>\n{html}\n</main>"))),
        (None, Some(mocks)) => choice(&title, mocks),
        _ => Err("Pass either html or mocks.".into()),
    }
}

fn shell(title: &str, head: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{title}</title>\n{head}\n</head>\n<body>\n{body}\n</body>\n</html>\n"
    )
}

/// A choice between mocks: one shown at a time, switched and picked from a floating dock at the bottom.
fn choice(title: &str, mocks: &[Value]) -> Result<String, String> {
    if mocks.is_empty() || mocks.len() > LETTERS.len() {
        return Err(format!("mocks takes 1 to {} mocks.", LETTERS.len()));
    }
    let (mut tabs, mut slides) = (String::new(), String::new());
    for (mock, letter) in mocks.iter().zip(LETTERS) {
        let id = letter.to_lowercase();
        let name = escape(mock["name"].as_str().ok_or("Each mock needs a name.")?);
        let difference = escape(mock["difference"].as_str().unwrap_or_default());
        let html = mock["html"].as_str().ok_or("Each mock needs html.")?;
        let width = match &mock["width"] {
            Value::Null => 1280,
            width => width.as_u64().ok_or("A mock's width is a whole number of px.")?,
        };
        tabs.push_str(&format!("    <a href=\"#mock-{id}\" title=\"{difference}\"><span class=\"l {id}\">{letter}</span>{name}</a>\n"));
        slides.push_str(&format!(
            "  <section class=\"slide\" id=\"mock-{id}\" aria-label=\"{letter}: {name}\" data-mock=\"{letter}\" data-difference=\"{difference}\">\n    <div class=\"view\"><div class=\"mock\" style=\"width: {width}px\">\n{html}\n    </div></div>\n  </section>\n"
        ));
    }
    let body = format!(
        "<main class=\"mocks\">\n{slides}</main>\n<div class=\"dock\" role=\"toolbar\" aria-label=\"Mocks\">\n  <span class=\"q\" title=\"{title}\">{title}</span>\n  <nav class=\"tabs\">\n{tabs}  </nav>\n  <span class=\"difference\" id=\"difference\"></span>\n  <button class=\"pick\" id=\"pick\">Pick A</button>\n</div>\n<script>\n{MOCKS_JS}</script>"
    );
    Ok(shell(title, MOCKS_HEAD, &body))
}

/// The tool result for what the page submitted: the picked mock by letter and name, or the page's JSON.
pub fn answer(input: &Value, submitted: &str) -> String {
    let value: Value = serde_json::from_str(submitted).unwrap_or_else(|_| json!(submitted));
    if let (Some(mocks), Some(letter)) = (input["mocks"].as_array(), value["mock"].as_str())
        && let Some(mock) = LETTERS.iter().position(|known| *known == letter).and_then(|index| mocks.get(index))
    {
        return format!("The user picked mock {letter}: {}.", mock["name"].as_str().expect("render requires a name"));
    }
    format!("The page submitted: {value}")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn send(url: &str, raw: impl FnOnce(&str, &str) -> String) -> String {
        let rest = url.strip_prefix("http://").unwrap();
        let (address, path) = rest.split_at(rest.find('/').unwrap());
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(raw(address, path).as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    }

    fn request(url: &str, method: &str, body: &str) -> String {
        send(url, |address, path| format!("{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\n\r\n{body}", body.len()))
    }

    #[test]
    fn serves_published_pages_only_under_the_token_and_takes_submits_only_from_the_waited_page() {
        let dir = std::env::temp_dir().join(format!("agent-pages-{}", std::process::id()));
        let next_session = dir.with_extension("next");
        let pages = Pages::start(Ipv4Addr::LOCALHOST.into(), None).unwrap();
        let first = pages.publish(&dir, "<p>one</p>", false).unwrap();
        let second = pages.publish(&dir, "<p>two</p>", false).unwrap();
        std::fs::remove_file(&first.path).unwrap();
        let third = pages.publish(&dir, "<p>three</p>", false).unwrap();
        let elsewhere = pages.publish(&next_session, "<p>four</p>", false).unwrap();
        let page = request(&second.url, "GET", "");
        let deleted = request(&first.url, "GET", "");
        let origin = pages.base.rsplit_once('/').unwrap().0;
        let without_token = request(&format!("{origin}/2"), "GET", "");
        let wrong_token = request(&format!("{origin}/{}/2", "0".repeat(32)), "GET", "");
        let traversal = request(&format!("{}/../2", pages.base), "GET", "");
        let unpublished = request(&format!("{}/7", pages.base), "GET", "");
        let nobody_waits = request(&second.url, "POST", "{\"x\":0}");
        let huge = send(&second.url, |address, path| format!("POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: 9000000000000000000\r\n\r\n"));
        let waited_page = pages.publish(&dir, "<form></form>", true).unwrap();
        let (other_page, submit) = (request(&third.url, "POST", "{\"x\":3}"), request(&waited_page.url, "POST", "{\"x\":5}"));
        // A refused submit never arrives, so the wait would block forever.
        let refused = AtomicBool::new(!submit.starts_with("HTTP/1.1 204 "));
        let waited = pages.wait(waited_page.number, &refused);
        let aborted = pages.wait(waited_page.number, &AtomicBool::new(true));
        let after_wait = request(&waited_page.url, "POST", "{\"x\":5}");
        let elsewhere_page = request(&elsewhere.url, "GET", "");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&next_session).ok();

        assert_eq!(third.path, dir.join("3.html"), "the deleted 1.html must not make the next page overwrite 2.html");
        assert_eq!((elsewhere.number, elsewhere.path.clone()), (4, next_session.join("1.html")), "a new session's 1.html must not take the URL of the first");
        assert!(elsewhere_page.ends_with("<p>four</p>") && deleted.starts_with("HTTP/1.1 404 "), "{elsewhere_page}{deleted}");
        assert!(page.starts_with("HTTP/1.1 200 OK\r\n") && page.ends_with("<p>two</p>"), "{page}");
        assert!(page.contains(&format!("Content-Security-Policy: {CSP}\r\n")));
        for refused in [without_token, wrong_token, traversal, unpublished] {
            assert!(refused.starts_with("HTTP/1.1 404 "), "{refused}");
        }
        assert!(nobody_waits.starts_with("HTTP/1.1 409 ") && after_wait.starts_with("HTTP/1.1 409 "), "{nobody_waits}{after_wait}");
        assert!(huge.starts_with("HTTP/1.1 413 "), "{huge}");
        assert!(other_page.starts_with("HTTP/1.1 409 ") && submit.starts_with("HTTP/1.1 204 "), "{other_page}{submit}");
        assert!(!submit.contains("Content-Length"), "{submit}");
        assert_eq!(waited.as_deref(), Some("{\"x\":5}"), "a submit sent before wait starts must still arrive");
        assert_eq!(aborted, None);
    }

    #[test]
    fn renders_a_document_or_a_mock_choice_and_names_the_picked_mock() {
        let document = render(&json!({"title": "A <b>", "html": "<p>x</p>"})).unwrap();
        let mocks = json!({"title": "Which?", "mocks": [{"name": "Dense", "html": "<i>a</i>"}, {"name": "Roomy", "width": 390, "html": "<i>b</i>"}]});
        let choice = render(&mocks).unwrap();

        assert!(document.contains("<h1>A &lt;b&gt;</h1>\n<p>x</p>") && document.contains("const agent") && document.contains("agent.chart = "));
        assert!(choice.contains("aria-label=\"B: Roomy\" data-mock=\"B\"") && choice.contains("style=\"width: 390px\""), "{choice}");
        assert!(!choice.contains("mock-c"));
        assert_eq!(render(&json!({"title": "t"})), Err("Pass either html or mocks.".into()));
        assert_eq!(render(&json!({"title": "t", "mocks": []})), Err("mocks takes 1 to 5 mocks.".into()));
        assert_eq!(render(&json!({"title": "t", "mocks": [{"name": "n", "html": "", "width": "390px"}]})), Err("A mock's width is a whole number of px.".into()));
        assert_eq!(answer(&mocks, "{\"mock\":\"B\"}"), "The user picked mock B: Roomy.");
        assert_eq!(answer(&mocks, "{\"mock\":\"E\"}"), "The page submitted: {\"mock\":\"E\"}");
        assert_eq!(answer(&json!({"title": "t", "html": ""}), "not json"), "The page submitted: \"not json\"");
    }
}
