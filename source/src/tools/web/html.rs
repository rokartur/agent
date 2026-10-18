// shortcut: no Readability scoring; the whole body minus page chrome is converted. Port a scorer if menus or ads swamp real pages.
// shortcut: tag scanner, not an HTML5 parser; unclosed block tags end at the next block.

const SKIPPED: [&str; 12] = ["script", "style", "nav", "header", "footer", "aside", "noscript", "svg", "template", "head", "iframe", "button"];

/// The page `<title>` and the body as Markdown. `base` resolves relative links.
pub fn to_markdown(html: &str, base: &str) -> (String, String) {
    let mut out = Writer::default();
    let mut title = String::new();
    let mut skip: Option<String> = None;
    let mut in_title = false;
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            text(rest, skip.is_some(), in_title, &mut title, &mut out);
            break;
        };
        text(&rest[..lt], skip.is_some(), in_title, &mut title, &mut out);
        rest = &rest[lt..];
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        let Some(gt) = tag_end(rest) else {
            text(rest, skip.is_some(), in_title, &mut title, &mut out);
            break;
        };
        let tag = Tag::parse(&rest[1..gt]);
        rest = &rest[gt + 1..];
        let Some(tag) = tag else { continue };
        // `<title>` lives in the skipped `<head>`; one inside an `<svg>` icon is not the page's.
        if tag.name == "title" && matches!(skip.as_deref(), None | Some("head")) {
            in_title = !tag.closing;
            continue;
        }
        if let Some(skipped) = &skip {
            // `</head>` is optional in HTML, so `<body>` ends the head too.
            if (tag.closing && tag.name == *skipped) || (skipped == "head" && tag.name == "body") {
                skip = None;
            }
            continue;
        }
        if SKIPPED.contains(&tag.name.as_str()) && !tag.closing && !tag.self_closing {
            // A `<` in script or style code would parse as a tag and could swallow the closing one.
            if matches!(tag.name.as_str(), "script" | "style") {
                let close = format!("</{}", tag.name);
                rest = &rest[rest.as_bytes().windows(close.len()).position(|w| w.eq_ignore_ascii_case(close.as_bytes())).unwrap_or(rest.len())..];
            }
            skip = Some(tag.name);
            continue;
        }
        if tag.name == "pre" && !tag.closing {
            let end = rest.as_bytes().windows(5).position(|w| w.eq_ignore_ascii_case(b"</pre")).unwrap_or(rest.len());
            out.code_block(&decode(&strip_tags(&rest[..end])));
            rest = &rest[end..];
            continue;
        }
        out.tag(&tag, base);
    }
    (decode(title.trim()).split_whitespace().collect::<Vec<_>>().join(" "), out.finish())
}

fn text(raw: &str, skipped: bool, in_title: bool, title: &mut String, out: &mut Writer) {
    if in_title {
        title.push_str(raw);
    } else if !skipped {
        out.inline(&decode(raw));
    }
}

/// The `>` closing the tag at the start of `rest`, skipping `>` inside quoted attribute values.
fn tag_end(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (index, ch) in rest.char_indices().skip(1) {
        match (quote, ch) {
            (None, '"' | '\'') => quote = Some(ch),
            (Some(open), _) if ch == open => quote = None,
            (None, '>') => return Some(index),
            _ => {}
        }
    }
    None
}

struct Tag {
    name: String,
    closing: bool,
    self_closing: bool,
    href: Option<String>,
}

impl Tag {
    fn parse(inner: &str) -> Option<Tag> {
        let closing = inner.starts_with('/');
        let inner = inner.trim_start_matches('/');
        let name: String = inner.chars().take_while(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase();
        if name.is_empty() {
            return None;
        }
        Some(Tag { closing, self_closing: inner.ends_with('/'), href: attribute(inner, "href"), name })
    }
}

fn attribute(inner: &str, name: &str) -> Option<String> {
    let lower = inner.to_ascii_lowercase();
    let mut from = 0;
    while let Some(found) = lower[from..].find(name) {
        let start = from + found;
        from = start + name.len();
        let before_ok = lower[..start].ends_with(|c: char| c.is_whitespace());
        let after = lower[from..].trim_start();
        if !before_ok || !after.starts_with('=') {
            continue;
        }
        let value = inner[inner.len() - after.len() + 1..].trim_start();
        return Some(match value.chars().next() {
            Some(quote @ ('"' | '\'')) => value[1..].split(quote).next().unwrap_or_default().to_string(),
            _ => value.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or_default().to_string(),
        });
    }
    None
}

/// Also cleans Brave snippets, which wrap matched terms in `<strong>`.
pub(super) fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut chars = html.chars().peekable();
    while let Some(ch) = chars.next() {
        let opens_tag = ch == '<' && chars.peek().is_some_and(|next| next.is_ascii_alphabetic() || matches!(next, '/' | '!'));
        match ch {
            _ if opens_tag => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

enum List {
    Bullet,
    Ordered(usize),
}

#[derive(Default)]
struct Writer {
    out: String,
    lists: Vec<List>,
    /// Output offset and target of each open `<a>`, wrapped into `[text](href)` on `</a>`.
    links: Vec<(usize, Option<String>)>,
    quotes: Vec<usize>,
    row_cells: usize,
    header_done: bool,
}

impl Writer {
    /// Text outside `<pre>`: whitespace runs collapse to one space, none at a line start.
    fn inline(&mut self, text: &str) {
        if text.starts_with(char::is_whitespace) {
            self.space();
        }
        let mut words = text.split_whitespace();
        if let Some(first) = words.next() {
            self.out.push_str(first);
            for word in words {
                self.out.push(' ');
                self.out.push_str(word);
            }
            if text.ends_with(char::is_whitespace) {
                self.space();
            }
        }
    }

    fn space(&mut self) {
        if !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
            self.out.push(' ');
        }
    }

    fn block(&mut self) {
        let trimmed = self.out.trim_end_matches(' ').len();
        self.out.truncate(trimmed);
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push_str(if self.out.ends_with('\n') { "\n" } else { "\n\n" });
        }
    }

    fn line(&mut self) {
        let trimmed = self.out.trim_end_matches(' ').len();
        self.out.truncate(trimmed);
        if !self.out.is_empty() && !self.out.ends_with('\n') {
            self.out.push('\n');
        }
    }

    fn code_block(&mut self, code: &str) {
        self.block();
        self.out.push_str("```\n");
        self.out.push_str(code.trim_matches('\n'));
        self.out.push_str("\n```\n\n");
    }

    fn tag(&mut self, tag: &Tag, base: &str) {
        let open = !tag.closing;
        match tag.name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block();
                if open {
                    let level = tag.name[1..].parse().unwrap_or(1);
                    self.out.push_str(&"#".repeat(level));
                    self.out.push(' ');
                }
            }
            "p" | "div" | "section" | "article" | "main" | "dl" | "figure" | "table" => {
                self.block();
                if tag.name == "table" {
                    self.header_done = false;
                }
            }
            "br" => self.out.push('\n'),
            "hr" => {
                self.block();
                self.out.push_str("---\n\n");
            }
            "ul" | "ol" => {
                if open {
                    self.line();
                    self.lists.push(if tag.name == "ol" { List::Ordered(0) } else { List::Bullet });
                } else {
                    self.lists.pop();
                    if self.lists.is_empty() {
                        self.block();
                    }
                }
            }
            "li" if open => {
                self.line();
                let depth = self.lists.len().saturating_sub(1);
                self.out.push_str(&"  ".repeat(depth));
                let marker = match self.lists.last_mut() {
                    Some(List::Ordered(n)) => {
                        *n += 1;
                        format!("{n}. ")
                    }
                    _ => "- ".to_string(),
                };
                self.out.push_str(&marker);
            }
            "dt" | "dd" if open => self.line(),
            "a" if open => {
                let href = tag.href.as_deref().filter(|href| !href.starts_with('#') && !href.starts_with("javascript:")).map(|href| resolve(base, &decode(href)));
                self.links.push((self.out.len(), href));
            }
            "a" => {
                if let Some((start, Some(href))) = self.links.pop() {
                    // A `</td>` inside the link may have trimmed `out` below `start`.
                    let start = self.out.floor_char_boundary(start.min(self.out.len()));
                    let label = self.out[start..].split_whitespace().collect::<Vec<_>>().join(" ");
                    self.out.truncate(start);
                    if !label.is_empty() {
                        self.out.push_str(&format!("[{label}]({href})"));
                    }
                }
            }
            "strong" | "b" => self.out.push_str("**"),
            "em" | "i" => self.out.push('*'),
            "code" => self.out.push('`'),
            "blockquote" if open => {
                self.block();
                self.quotes.push(self.out.len());
            }
            "blockquote" => {
                if let Some(start) = self.quotes.pop() {
                    let start = self.out.floor_char_boundary(start.min(self.out.len()));
                    let quoted: Vec<String> = self.out[start..].trim().lines().map(|line| format!("> {line}").trim_end().to_string()).collect();
                    self.out.truncate(start);
                    self.out.push_str(&quoted.join("\n"));
                    self.block();
                }
            }
            "tr" if open => {
                self.line();
                self.out.push('|');
                self.row_cells = 0;
            }
            "tr" => {
                if !self.header_done && self.row_cells > 0 {
                    self.out.push('\n');
                    self.out.push('|');
                    self.out.push_str(&" --- |".repeat(self.row_cells));
                    self.header_done = true;
                }
                self.out.push('\n');
            }
            "td" | "th" if open => self.out.push(' '),
            "td" | "th" => {
                let trimmed = self.out.trim_end().len();
                self.out.truncate(trimmed);
                self.out.push_str(" |");
                self.row_cells += 1;
            }
            _ => {}
        }
    }

    fn finish(self) -> String {
        let mut lines: Vec<&str> = Vec::new();
        for line in self.out.lines().map(str::trim_end) {
            if line.is_empty() && lines.last().is_none_or(|last| last.is_empty()) {
                continue;
            }
            lines.push(line);
        }
        lines.join("\n").trim().to_string()
    }
}

/// `href` as an absolute URL against the page URL `base`.
pub fn resolve(base: &str, href: &str) -> String {
    let has_scheme = href.split_once(':').is_some_and(|(scheme, _)| scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c)));
    if has_scheme {
        return href.to_string();
    }
    let scheme_end = base.find("://").map_or(0, |i| i + 3);
    if let Some(rest) = href.strip_prefix("//") {
        return format!("{}{rest}", &base[..scheme_end]);
    }
    let origin_end = base[scheme_end..].find('/').map_or(base.len(), |i| scheme_end + i);
    if href.starts_with('/') {
        return format!("{}{href}", &base[..origin_end]);
    }
    let path = base[origin_end..].split(['?', '#']).next().unwrap_or_default();
    if href.starts_with('?') {
        return format!("{}{path}{href}", &base[..origin_end]);
    }
    let dir = path.rfind('/').map_or("/", |i| &path[..=i]);
    format!("{}{dir}{href}", &base[..origin_end])
}

pub fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let entity = rest[1..].find(';').filter(|end| *end <= 10).map(|end| &rest[1..=end]);
        match entity.and_then(entity_char) {
            Some(ch) => {
                out.push(ch);
                rest = &rest[entity.map_or(0, str::len) + 2..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity_char(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        // Digits only: `from_str_radix` and `parse` also take a sign, `&#+65;`.
        let code = match number.strip_prefix(['x', 'X']) {
            Some(hex) if hex.bytes().all(|b| b.is_ascii_hexdigit()) => u32::from_str_radix(hex, 16).ok()?,
            None if number.bytes().all(|b| b.is_ascii_digit()) => number.parse().ok()?,
            _ => return None,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "copy" => '©',
        "reg" => '®',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_keeps_headings_paragraphs_and_links_and_drops_chrome() {
        let html = r#"<html><head><title>Rust &amp; Web</title><style>p{}</style></head><body>
            <nav><a href="/">Home</a></nav><header>Site</header>
            <h1>Intro</h1><p>Read the <a href="/docs/guide.html">guide</a> and <strong>enjoy</strong>.</p>
            <script>alert("x > y")</script><aside>Ads</aside><p>Second&nbsp;para &lt;ok&gt;</p>
            <footer>Copyright</footer></body></html>"#;
        let (title, markdown) = to_markdown(html, "https://example.com/blog/post");
        assert_eq!(title, "Rust & Web");
        assert_eq!(markdown, "# Intro\n\nRead the [guide](https://example.com/docs/guide.html) and **enjoy**.\n\nSecond para <ok>");
    }

    #[test]
    fn lists_and_code_blocks() {
        let html = "<ul><li>one</li><li>two<ol><li>a</li><li>b</li></ol></li></ul><p>Run <code>cargo test</code>:</p><pre><code>fn main() {\n    if a < b {}\n}</code></pre>";
        let (_, markdown) = to_markdown(html, "https://example.com/");
        assert_eq!(markdown, "- one\n- two\n  1. a\n  2. b\n\nRun `cargo test`:\n\n```\nfn main() {\n    if a < b {}\n}\n```");
    }

    #[test]
    fn tables_become_pipe_tables() {
        let table = "<table><tr><th>Name</th><th>Size</th></tr><tr><td>a.rs</td><td>3 KB</td></tr></table>";
        let (_, markdown) = to_markdown(&format!("{table}<p>After</p>{table}"), "https://example.com/");
        let expected = "| Name | Size |\n| --- | --- |\n| a.rs | 3 KB |";
        assert_eq!(markdown, format!("{expected}\n\nAfter\n\n{expected}"));
    }

    #[test]
    fn misnested_tags_do_not_panic_and_links_stay_on_one_line() {
        assert_eq!(to_markdown("<table><tr><td>t<br><br><br><a href=x></td></a>", "https://a.dev/").1, "| t |");
        assert_eq!(to_markdown("x<a href=\"/y\"><blockquote></a></blockquote>", "https://a.dev/").1, "x");
        assert_eq!(to_markdown("<a href=/p><h3>Title</h3><p>Desc</p></a>", "https://a.dev/").1, "[### Title Desc](https://a.dev/p)");
    }

    #[test]
    fn a_form_keeps_its_content_and_an_svg_title_is_not_the_page_title() {
        let html = "<head><title>Docs</title></head><body><svg><title>Close menu</title></svg><form id=f><h1>Guide</h1><p>Text</p></form></body>";
        assert_eq!(to_markdown(html, "https://a.dev/"), ("Docs".into(), "# Guide\n\nText".into()));
    }

    #[test]
    fn script_code_and_a_missing_head_end_do_not_swallow_the_body() {
        assert_eq!(to_markdown("<script>for(i=0;i<n;i++){}</script><p>Body</p>", "https://a.dev/").1, "Body");
        assert_eq!(to_markdown("<head><title>T</title><body><p>Body</p>", "https://a.dev/").1, "Body");
    }

    #[test]
    fn numeric_entities_take_digits_only() {
        assert_eq!(decode("&#65;&#x41;&#+65;&#x+41;"), "AA&#+65;&#x+41;");
    }

    #[test]
    fn hrefs_resolve_like_a_browser() {
        let base = "https://a.dev/docs/page?x=1";
        assert_eq!(resolve(base, "/out?to=https://x.dev"), "https://a.dev/out?to=https://x.dev");
        assert_eq!(resolve(base, "tel:123"), "tel:123");
        assert_eq!(resolve(base, "?page=2"), "https://a.dev/docs/page?page=2");
        assert_eq!(resolve(base, "guide.html"), "https://a.dev/docs/guide.html");
        assert_eq!(resolve(base, "//b.dev/x"), "https://b.dev/x");
    }
}
