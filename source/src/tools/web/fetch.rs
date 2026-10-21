use super::{MAX_INLINE_CHARS, Page, Web, fetch_entry, github, html, http_agent};
use serde_json::Value;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::time::Duration;

const MAX_RESPONSE_BYTES: u64 = 5 * 1024 * 1024;
const MAX_REDIRECTS: usize = 10;
const ACCEPT: &str = "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8";
const USER_AGENT: &str = "Mozilla/5.0 (compatible; agent)";

impl Web {
    pub(super) fn fetch_content(&mut self, input: &Value) -> Result<String, String> {
        let mut urls = super::string_list(&input["urls"]);
        if urls.is_empty()
            && let Some(url) = input["url"].as_str()
        {
            urls.push(url.to_string());
        }
        urls.retain(|u| !u.trim().is_empty());
        if urls.is_empty() {
            return Err("Error: No URL provided. Use the 'url' parameter, or 'urls' for parallel fetches.".into());
        }
        // One thread per URL, each buffering up to `MAX_RESPONSE_BYTES`.
        if urls.len() > super::MAX_FETCHED_PAGES {
            return Err(format!("Error: at most {} URLs per call.", super::MAX_FETCHED_PAGES));
        }
        let raw = input["mode"].as_str() == Some("raw");
        let pages = fetch_all(&urls, raw, input["forceClone"].as_bool().unwrap_or(false));
        if let [page] = pages.as_slice()
            && let Some(error) = &page.error
        {
            return Err(format!("Error: {error}"));
        }
        let id = self.store(fetch_entry(&pages))?;
        if let [page] = pages.as_slice() {
            return Ok(first_page(&page.content, &id));
        }
        let mut output = String::from("## Fetched URLs\n\n");
        for page in &pages {
            match &page.error {
                Some(error) => output.push_str(&format!("- {}: Error - {error}\n", page.url)),
                None => {
                    let name = if page.title.is_empty() { &page.url } else { &page.title };
                    output.push_str(&format!("- {name} ({} chars)\n", page.content.chars().count()));
                }
            }
        }
        output.push_str(&format!("\n---\nUse GetSearchContent({{ responseId: \"{id}\", urlIndex: 0 }}) to retrieve bounded content slices."));
        Ok(output)
    }
}

/// pi's initialContentSlice: up to MAX_INLINE_CHARS, cut after a newline in the last fifth when one is there.
fn first_page(content: &str, id: &str) -> String {
    let length = content.chars().count();
    if length <= MAX_INLINE_CHARS {
        return content.to_string();
    }
    let mut end = MAX_INLINE_CHARS;
    let head = super::char_slice(content, 0, end);
    if let Some(newline) = head.rfind('\n') {
        let line_end = head[..newline].chars().count();
        if line_end >= MAX_INLINE_CHARS * 4 / 5 {
            end = line_end + 1;
        }
    }
    let page = super::char_slice(content, 0, end);
    format!(
        "{page}\n\n---\nShowing {end} of {length} chars, {} of {} bytes, and {} of {} lines. Use GetSearchContent({{ responseId: \"{id}\", urlIndex: 0, offset: {end} }}) for the next slice.",
        page.len(),
        content.len(),
        page.lines().count(),
        content.lines().count()
    )
}

/// Fetches every URL on its own thread; each page carries its error instead of failing the batch.
pub fn fetch_all(urls: &[String], raw: bool, force_clone: bool) -> Vec<Page> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = urls.iter().map(|url| scope.spawn(move || fetch_one(url, raw, force_clone))).collect();
        let pages = handles.into_iter().zip(urls).map(|(handle, url)| {
            handle.join().unwrap_or_else(|_| Err("fetch thread panicked".into())).map_or_else(
                |error| Page { url: url.clone(), title: String::new(), content: String::new(), error: Some(error) },
                |(title, content)| Page { url: url.clone(), title, content, error: None },
            )
        });
        pages.collect()
    })
}

/// (title, content) of one URL; GitHub code URLs are cloned, the rest go over HTTP.
fn fetch_one(url: &str, raw: bool, force_clone: bool) -> Result<(String, String), String> {
    let url = url.trim();
    let scheme = url.split_once("://").map(|(scheme, _)| scheme.to_ascii_lowercase());
    if !matches!(scheme.as_deref(), Some("http" | "https")) {
        return Err(format!("Unsupported URL: {url}. Use an http or https URL."));
    }
    let proxied = ureq::Proxy::try_from_env().is_some();
    refuse_local(url, proxied)?;
    let mut clone_error = None;
    if !raw && let Some(repo) = github::parse(url) {
        // shortcut: a failed clone falls back to the plain page fetch, pi tries the GitHub API first.
        match github::fetch(&repo, force_clone) {
            Ok(content) => return Ok((format!("{}/{}", repo.owner, repo.repo), content)),
            Err(error) => clone_error = Some(error),
        }
    }
    let (title, content) = fetch_http(&http_agent(Duration::from_secs(30)), url, raw, |hop| refuse_local(hop, proxied))?;
    match clone_error {
        Some(error) => Ok((title, format!("{content}\n\n(The git clone failed, so this is the page: {error})"))),
        None => Ok((title, content)),
    }
}

/// fetch_content may run unasked in auto mode, and web_search fetches result URLs without asking, so no
/// fetched URL may reach cloud metadata, routers or local dev servers.
/// shortcut: DNS rebinding between this check and the request is not covered, and behind a proxy a name this machine
/// cannot resolve passes (the proxy may resolve internal names); pin the resolved address if that matters.
fn refuse_local(url: &str, proxied: bool) -> Result<(), String> {
    let refused = || Err(format!("Refusing to fetch a local or private address: {url}. Use bash (curl) for local servers."));
    let host = super::host(url).unwrap_or_default();
    if host == "localhost" || host.ends_with(".localhost") {
        return refused();
    }
    match (host.as_str(), 0).to_socket_addrs() {
        Ok(mut addresses) => {
            if addresses.any(|address| is_local(&address.ip())) {
                refused()
            } else {
                Ok(())
            }
        }
        Err(error) if !proxied => Err(format!("Cannot resolve the host of {url}: {error}")),
        Err(_) => Ok(()),
    }
}

fn is_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [first, second, ..] = v4.octets();
            let shared = first == 100 && second & 0xc0 == 64;
            let benchmarking = first == 198 && second & 0xfe == 18;
            // Linux routes 0.0.0.0/8 to this host.
            let this_network = first == 0;
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_broadcast() || v4.is_multicast() || shared || benchmarking || this_network
        }
        IpAddr::V6(v6) => match embedded_v4(v6) {
            Some(v4) => is_local(&IpAddr::V4(v4)),
            None => {
                let site_local = v6.segments()[0] & 0xffc0 == 0xfec0;
                // RFC 8215 local-use NAT64; the IPv4 part can sit at any of several offsets, so refuse the whole /48.
                let local_nat64 = v6.segments()[..3] == [0x64, 0xff9b, 1];
                v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local() || v6.is_unicast_link_local() || v6.is_multicast() || site_local || local_nat64
            }
        },
    }
}

/// The IPv4 address an IPv4-mapped, IPv4-compatible, NAT64 (`64:ff9b::/96`) or 6to4 (`2002::/16`) address reaches.
fn embedded_v4(v6: &Ipv6Addr) -> Option<Ipv4Addr> {
    let v4 = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    match v6.segments() {
        [0x64, 0xff9b, 0, 0, 0, 0, high, low] | [0x2002, high, low, ..] => Some(v4(high, low)),
        _ => v6.to_ipv4(),
    }
}

/// (title, content) of an HTTP GET. `allow` vets the URL and every redirect target before it is requested.
fn fetch_http(agent: &ureq::Agent, url: &str, raw: bool, allow: impl Fn(&str) -> Result<(), String>) -> Result<(String, String), String> {
    let mut final_url = url.to_string();
    let mut redirects = 0;
    let mut response = loop {
        allow(&final_url)?;
        let response = agent.get(&final_url).header("Accept", ACCEPT).header("User-Agent", USER_AGENT).call().map_err(|e| e.to_string())?;
        let location = response.headers().get("location").and_then(|v| v.to_str().ok());
        let Some(location) = location.filter(|_| response.status().is_redirection()) else { break response };
        if redirects == MAX_REDIRECTS {
            return Err(format!("Too many redirects (over {MAX_REDIRECTS})"));
        }
        redirects += 1;
        final_url = html::resolve(&final_url, location);
    };
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}: {}", status.as_u16(), status.canonical_reason().unwrap_or_default()));
    }
    let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or_default();
    let mime = content_type.split(';').next().unwrap_or_default().trim().to_lowercase();
    if raw && !is_text(&mime) {
        return Err(format!("Unsupported content type in raw mode: {}", if mime.is_empty() { "missing" } else { &mime }));
    }
    if !mime.is_empty() && !is_text(&mime) {
        return Err(format!("Unsupported content type: {mime}"));
    }
    let mut bytes = Vec::new();
    response.body_mut().as_reader().take(MAX_RESPONSE_BYTES + 1).read_to_end(&mut bytes).map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_RESPONSE_BYTES {
        return Err(format!("Response too large (over {}MB)", MAX_RESPONSE_BYTES / 1024 / 1024));
    }
    // shortcut: non-UTF-8 pages decode lossily, the charset parameter is ignored; decode it if such pages show up.
    let body = String::from_utf8_lossy(&bytes).into_owned();
    if !raw && (mime == "text/html" || mime.ends_with("xhtml+xml")) {
        return Ok(html::to_markdown(&body, &final_url));
    }
    Ok((text_title(&body, &final_url), body))
}

fn is_text(mime: &str) -> bool {
    mime.starts_with("text/") || mime.ends_with("+json") || mime.ends_with("+xml") || ["application/json", "application/xml", "application/javascript", "application/x-javascript"].contains(&mime)
}

/// pi's extractTextTitle: the first `#` or `##` heading, else the last path segment, else the URL.
fn text_title(text: &str, url: &str) -> String {
    for line in text.lines() {
        let hashes = line.len() - line.trim_start_matches('#').len();
        if (1..=2).contains(&hashes) && line[hashes..].starts_with(char::is_whitespace) {
            let title = line[hashes..].trim().replace('*', "");
            if !title.is_empty() {
                return title;
            }
        }
    }
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let path = path.split_once("://").map_or(path, |(_, rest)| rest);
    match path.split_once('/') {
        Some((_, path)) => path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(url).to_string(),
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_page_cuts_at_a_late_newline_and_points_to_the_rest() {
        let content = format!("{}\n{}", "a".repeat(29_000), "b".repeat(2_000));
        let page = first_page(&content, "x1");
        assert!(page.starts_with(&format!("{}\n\n\n---\nShowing 29001 of 31001 chars, 29001 of 31001 bytes, and 1 of 2 lines.", "a".repeat(29_000))), "{}", &page[29_000..]);
        assert!(page.ends_with("GetSearchContent({ responseId: \"x1\", urlIndex: 0, offset: 29001 }) for the next slice."));
        assert_eq!(first_page("short", "x1"), "short");
    }

    #[test]
    fn text_title_prefers_a_heading_then_the_path() {
        assert_eq!(text_title("intro\n## **Usage** guide\n", "https://a.dev/x"), "Usage guide");
        assert_eq!(text_title("### deep\nplain", "https://a.dev/docs/notes.txt?v=1"), "notes.txt");
        assert_eq!(text_title("", "https://a.dev"), "https://a.dev");
    }

    #[test]
    fn raw_mode_accepts_text_like_types_only() {
        assert!(is_text("application/ld+json") && is_text("text/plain") && is_text("application/xhtml+xml"));
        assert!(!is_text("image/png") && !is_text(""));
    }

    #[test]
    fn local_and_private_addresses_are_refused() {
        for url in ["http://127.0.0.1:5173/", "HTTP://169.254.169.254/latest/meta-data/", "http://[::ffff:10.0.0.1]/", "http://LOCALHOST/", "http://a.localhost/"] {
            assert!(fetch_one(url, false, false).unwrap_err().starts_with("Refusing to fetch a local or private address"), "{url}");
        }
        assert!(refuse_local("https://93.184.215.14/", false).is_ok());
        for ip in ["100.64.0.1", "64:ff9b::a9fe:a9fe", "2002:7f00:1::"] {
            assert!(is_local(&ip.parse().unwrap()), "{ip}");
        }
        for ip in ["0.1.2.3", "255.255.255.255", "224.0.0.1", "198.18.0.1", "::7f00:1", "fec0::1", "ff02::1", "64:ff9b:1::a00:1"] {
            assert!(is_local(&ip.parse().unwrap()), "{ip}");
        }
        assert!(!is_local(&"2002:5db8:d70e::".parse().unwrap()));
    }

    #[test]
    fn an_unresolvable_host_is_refused_unless_a_proxy_resolves_it() {
        assert!(refuse_local("http://nothing.invalid/", false).unwrap_err().starts_with("Cannot resolve the host of"));
        assert!(refuse_local("http://nothing.invalid/", true).is_ok());
    }

    /// Answers each request with the response `route` gives for its path.
    fn serve(route: fn(&str) -> String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let mut request = [0; 4096];
                let read = stream.read(&mut request).unwrap();
                let path = String::from_utf8_lossy(&request[..read]).split_whitespace().nth(1).unwrap_or_default().to_string();
                _ = std::io::Write::write_all(&mut stream, route(&path).as_bytes());
            }
        });
        base
    }

    fn response(status: &str, headers: &str, body: &str) -> String {
        format!("HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
    }

    #[test]
    fn links_resolve_against_the_redirected_url_and_errors_or_binaries_are_refused() {
        let base = serve(|path| match path {
            "/docs" => response("302 Found", "location: /v2/\r\n", ""),
            "/v2/" => response("200 OK", "content-type: text/html\r\n", "<a href=\"guide.html\">Guide</a>"),
            "/doc.pdf" => response("200 OK", "content-type: application/pdf\r\n", "%PDF-1.4"),
            "/loop" => response("302 Found", "location: /loop\r\n", ""),
            "/to-secret" => response("301 Moved Permanently", "location: /secret\r\n", ""),
            "/secret" => response("200 OK", "content-type: text/plain\r\n", "token"),
            _ => response("404 Not Found", "content-type: text/plain\r\n", "Not Found"),
        });
        let agent = http_agent(Duration::from_secs(5));
        let fetch = |path: &str, raw: bool| fetch_http(&agent, &format!("{base}{path}"), raw, |hop| if hop.ends_with("/secret") { Err(format!("refused {hop}")) } else { Ok(()) });
        let (_, content) = fetch("/docs", false).unwrap();
        assert_eq!(content, format!("[Guide]({base}/v2/guide.html)"));
        assert_eq!(fetch("/doc.pdf", false).unwrap_err(), "Unsupported content type: application/pdf");
        assert_eq!(fetch("/x.txt", true).unwrap_err(), "HTTP 404: Not Found");
        assert_eq!(fetch("/loop", false).unwrap_err(), "Too many redirects (over 10)");
        assert_eq!(fetch("/to-secret", false).unwrap_err(), format!("refused {base}/secret"));
    }
}
