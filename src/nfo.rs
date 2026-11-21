//! Reads the title, plot and studio from the Jellyfin/Kodi `.nfo` file beside a video (`clip.mp4` -> `clip.nfo`).
//!
//! Read-only: the player never writes an NFO. Only the three tags it shows are looked for, so no XML library is needed.

use std::path::Path;

#[derive(Debug, Default, PartialEq)]
pub struct Nfo {
    pub title: String,
    pub plot: String,
    pub studio: String,
}

/// The NFO beside `video`, or None when there is none. A file that exists but cannot be read is
/// reported in player.log (and treated as no NFO), never silently skipped.
pub fn read_beside(video: &Path) -> Option<Nfo> {
    let path = video.with_extension("nfo");
    let started = std::time::Instant::now();
    let read = std::fs::read(&path);
    crate::diag::slow(&format!("reading {}", path.display()), started, 200);
    match read {
        Ok(bytes) => Some(parse(&String::from_utf8_lossy(&bytes))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            crate::diag::log(&format!("cannot read {}: {e}", path.display()));
            None
        }
    }
}

pub fn parse(xml: &str) -> Nfo {
    Nfo {
        title: tag_text(xml, "title"),
        plot: tag_text(xml, "plot"),
        studio: tag_text(xml, "studio"),
    }
}

/// Text of the first `<name>...</name>`, entities decoded and white space collapsed; empty when the tag
/// is missing, empty, self-closed or never closed. `<sorttitle>` is not `<title>`.
fn tag_text(xml: &str, name: &str) -> String {
    let open = format!("<{name}");
    let close = format!("</{name}>");
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open) {
        let start = from + i + open.len();
        let rest = &xml[start..];
        if !matches!(rest.chars().next(), Some('>' | '/' | ' ' | '\t' | '\r' | '\n')) {
            from = start; // `<titlex`: a different tag
            continue;
        }
        let Some(gt) = rest.find('>') else { return String::new() };
        if rest[..gt].ends_with('/') {
            return String::new();
        }
        let body = &rest[gt + 1..];
        return match body.find(&close) {
            Some(end) => clean(&body[..end]),
            None => String::new(),
        };
    }
    String::new()
}

/// CDATA sections are kept as they are, the rest has its entities decoded; then white space is collapsed.
fn clean(body: &str) -> String {
    let mut out = String::new();
    let mut rest = body;
    while let Some(i) = rest.find("<![CDATA[") {
        out.push_str(&decode(&rest[..i]));
        let after = &rest[i + "<![CDATA[".len()..];
        match after.find("]]>") {
            Some(end) => {
                out.push_str(&after[..end]);
                rest = &after[end + 3..];
            }
            None => {
                out.push_str(after);
                rest = "";
            }
        }
    }
    out.push_str(&decode(rest));
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match rest.find(';').filter(|&j| j <= 10) {
            Some(j) => {
                match entity(&rest[1..j]) {
                    Some(c) => out.push(c),
                    None => out.push_str(&rest[..=j]), // unknown entity: keep it as written
                }
                rest = &rest[j + 1..];
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

fn entity(e: &str) -> Option<char> {
    match e {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let n = e.strip_prefix('#')?;
            let code = match n.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

/// Break `text` into lines no wider than `max_w` (as `width` measures them), at most `max_lines`.
/// When text is left over, the last line ends in "…". A single word wider than a line is cut.
pub fn wrap(text: &str, max_w: f32, max_lines: usize, mut width: impl FnMut(&str) -> f32) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    if max_lines == 0 {
        return lines;
    }
    let mut cur = String::new();
    let mut cut = false;
    'words: for word in text.split_whitespace() {
        let mut word = word.to_string();
        loop {
            let cand = if cur.is_empty() { word.clone() } else { format!("{cur} {word}") };
            if width(&cand) <= max_w {
                cur = cand;
                continue 'words;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
                if lines.len() == max_lines {
                    cut = true;
                    break 'words;
                }
                continue; // try the word again on a fresh line
            }
            while word.chars().count() > 1 && width(&word) > max_w {
                word.pop();
            }
            cur = word;
            continue 'words;
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if cut {
        if let Some(last) = lines.last_mut() {
            while last.chars().count() > 1 && width(&format!("{last}…")) > max_w {
                last.pop();
            }
            last.push('…');
        }
    }
    lines
}

#[cfg(test)]
mod nfo_tests {
    use super::*;

    const JELLYFIN: &str = r#"<?xml version="1.0" encoding="utf-8" standalone="yes"?>
<movie>
  <plot />
  <lockdata>false</lockdata>
  <title>Tom &amp; Jerry &#8211; Round &#x32;</title>
  <originaltitle>Other</originaltitle>
  <sorttitle>zzz</sorttitle>
  <studio>Acme</studio>
  <studio>Second</studio>
  <actor><name>Someone</name></actor>
</movie>"#;

    #[test]
    fn reads_title_and_studio_and_decodes_entities() {
        let n = parse(JELLYFIN);
        assert_eq!(n.title, "Tom & Jerry \u{2013} Round 2");
        assert_eq!(n.studio, "Acme");
        assert_eq!(n.plot, "", "a self-closed <plot /> is empty");
    }

    #[test]
    fn sorttitle_and_originaltitle_are_not_the_title() {
        let n = parse("<movie><sorttitle>S</sorttitle><originaltitle>O</originaltitle></movie>");
        assert_eq!(n.title, "");
        assert_eq!(parse("<movie><sorttitle>S</sorttitle><title>T</title></movie>").title, "T");
    }

    #[test]
    fn plot_in_cdata_keeps_its_text_and_collapses_white_space() {
        let n = parse("<movie><plot>\n  <![CDATA[One & <b>two</b>\n\n three]]>  &lt;end&gt;\n</plot></movie>");
        assert_eq!(n.plot, "One & <b>two</b> three <end>");
    }

    #[test]
    fn broken_input_gives_empty_fields_not_a_panic() {
        assert_eq!(parse(""), Nfo::default());
        assert_eq!(parse("<movie><title>no end"), Nfo::default());
        assert_eq!(parse("<movie><title").title, "");
        assert_eq!(parse("<title>&bogus; & &#xZZ; &#99999999999;</title>").title, "&bogus; & &#xZZ; &#99999999999;");
        assert_eq!(parse("<title>caf\u{e9} \u{1f600}</title>").title, "caf\u{e9} \u{1f600}");
    }

    #[test]
    fn read_beside_finds_the_same_stem_and_reports_a_missing_file_as_none() {
        let dir = std::env::temp_dir().join(format!("equirect-nfo-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clip.nfo"), "<movie><title>Hi</title></movie>").unwrap();
        assert_eq!(read_beside(&dir.join("clip.mp4")).unwrap().title, "Hi");
        assert!(read_beside(&dir.join("other.mp4")).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn w(s: &str) -> f32 { s.chars().count() as f32 * 10.0 }

    #[test]
    fn wrap_breaks_at_words_and_fits_the_width() {
        let lines = wrap("aaa bbb ccc ddd eee", 75.0, 5, w);
        assert_eq!(lines, ["aaa bbb", "ccc ddd", "eee"]);
        assert!(lines.iter().all(|l| w(l) <= 75.0));
    }

    #[test]
    fn wrap_cuts_with_an_ellipsis_when_the_text_does_not_fit() {
        let lines = wrap("aaa bbb ccc ddd eee", 75.0, 2, w);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "aaa bbb");
        assert!(lines[1].ends_with('…') && w(&lines[1]) <= 75.0, "{lines:?}");
        assert_eq!(wrap("aaa bbb", 75.0, 1, w), ["aaa bbb"], "no ellipsis when everything fits");
    }

    #[test]
    fn wrap_handles_a_long_word_empty_text_and_no_lines() {
        let lines = wrap("abcdefghijklmnop z", 50.0, 3, w);
        assert!(lines[0].chars().count() <= 5 && lines.iter().all(|l| w(l) <= 50.0), "{lines:?}");
        assert!(wrap("   ", 50.0, 3, w).is_empty());
        assert!(wrap("abc", 50.0, 0, w).is_empty());
    }
}
