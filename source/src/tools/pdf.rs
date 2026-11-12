use std::io::Read;
use std::path::Path;

use serde_json::Value;

const DEFAULT_PAGES: usize = 20;
const MAX_BYTES: usize = 50_000;

/// By its first bytes, as image.rs does: an extension can lie.
pub fn is_pdf(path: &Path) -> bool {
    let mut start = [0; 5];
    // is_file first: opening a FIFO would block.
    path.is_file() && std::fs::File::open(path).and_then(|mut file| file.read_exact(&mut start)).is_ok() && &start == b"%PDF-"
}

/// The text of the pages asked for, through poppler's pdftotext. No anchors: a PDF is not edited by line.
pub fn read(path: &Path, input: &Value) -> Result<String, String> {
    let (first, last) = page_range(input["pages"].as_str())?;
    if !crate::process::installed("pdftotext") {
        return Err("Reading a PDF needs pdftotext. Install poppler: brew install poppler, apt-get install poppler-utils, or choco install poppler.".into());
    }
    let output =
        crate::process::command("pdftotext").args(["-layout", "-f", &first.to_string(), "-l", &last.to_string()]).arg(path).arg("-").output().map_err(|e| format!("Cannot run pdftotext: {e}"))?;
    if !output.status.success() {
        return Err(format!("pdftotext could not read {}: {}", path.display(), String::from_utf8_lossy(&output.stderr).trim()));
    }
    Ok(render(path, first, last, &String::from_utf8_lossy(&output.stdout)))
}

/// `"3"` or `"2-9"`, 1-based; None gives the first DEFAULT_PAGES.
fn page_range(pages: Option<&str>) -> Result<(usize, usize), String> {
    let Some(pages) = pages else { return Ok((1, DEFAULT_PAGES)) };
    let (first, last) = pages.split_once('-').unwrap_or((pages, pages));
    match (first.trim().parse(), last.trim().parse()) {
        (Ok(first), Ok(last)) if first >= 1 && last >= first => Ok((first, last)),
        _ => Err("[E_BAD_SHAPE] \"pages\" must be a page or a range, e.g. \"3\" or \"2-9\".".into()),
    }
}

/// pdftotext ends each page with a form feed.
fn render(path: &Path, first: usize, last: usize, text: &str) -> String {
    let pages: Vec<&str> = text.strip_suffix('\x0c').unwrap_or(text).split('\x0c').collect();
    let path = path.display();
    if pages.iter().all(|page| page.trim().is_empty()) {
        return format!("[No text on pages {first}-{last} of {path}: a scanned PDF. Render the pages with: pdftoppm -png -r 100 -f {first} -l {last} {path} /tmp/page, then Read the PNG files.]");
    }
    let mut out = format!("[PDF {path}, pages {first}-{}]\n", first + pages.len() - 1);
    for (index, page) in pages.iter().enumerate() {
        out.push_str(&format!("--- page {} ---\n{}\n", first + index, page.trim_end()));
    }
    if out.len() > MAX_BYTES {
        out.truncate((0..=MAX_BYTES).rev().find(|at| out.is_char_boundary(*at)).unwrap_or(0));
        out.push_str("\n[Cut at 50KB: ask for fewer pages.]");
    } else if pages.len() == last - first + 1 {
        out.push_str(&format!("[More pages may follow: pages \"{}-{}\".]", last + 1, last + pages.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_is_one_page_or_a_range() {
        assert_eq!(page_range(None), Ok((1, 20)));
        assert_eq!(page_range(Some("3")), Ok((3, 3)));
        assert_eq!(page_range(Some("2-9")), Ok((2, 9)));
        for bad in ["0", "9-2", "a", "1-2-3", "", "-3"] {
            assert!(page_range(Some(bad)).unwrap_err().starts_with("[E_BAD_SHAPE]"), "{bad}");
        }
    }

    #[test]
    fn pdf_text_is_split_into_pages_and_says_when_more_may_follow() {
        let path = Path::new("/a/b.pdf");
        assert_eq!(render(path, 2, 3, "one\n\x0ctwo\n\x0c"), "[PDF /a/b.pdf, pages 2-3]\n--- page 2 ---\none\n--- page 3 ---\ntwo\n[More pages may follow: pages \"4-5\".]");
        assert_eq!(render(path, 1, 20, "only\n\x0c"), "[PDF /a/b.pdf, pages 1-1]\n--- page 1 ---\nonly\n");
        assert!(render(path, 1, 2, "\x0c \n\x0c").starts_with("[No text on pages 1-2 of /a/b.pdf: a scanned PDF. Render the pages with: pdftoppm -png -r 100 -f 1 -l 2 /a/b.pdf /tmp/page"));
        let long = render(path, 1, 1, &format!("{}\x0c", "ż".repeat(40_000)));
        assert!(long.len() <= MAX_BYTES + 40 && long.ends_with("\n[Cut at 50KB: ask for fewer pages.]"), "{}", long.len());
    }

    #[test]
    fn a_pdf_is_known_by_its_first_bytes() {
        let dir = std::env::temp_dir().join(format!("agent-pdf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "%PDF-1.7\n").unwrap();
        std::fs::write(dir.join("b.pdf"), "text").unwrap();
        let found = [is_pdf(&dir.join("a.txt")), is_pdf(&dir.join("b.pdf")), is_pdf(&dir.join("none"))];
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(found, [true, false, false]);
    }
}
