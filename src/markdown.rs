//! Markdown in, HTML out, and back again for display.
//!
//! `description_html` is the field Plane stores, so markdown written straight
//! into it is kept literally: asterisks stay asterisks. Everything this CLI
//! writes therefore goes through [`to_html`] first. Reading back, [`to_text`]
//! flattens the stored HTML so `plane issue get` prints prose rather than
//! Plane's editor markup.

use pulldown_cmark::{html, Event, Options, Parser};

/// Convert markdown to the HTML Plane stores in `description_html`.
///
/// Raw HTML in the source is neutralised rather than passed through: an agent
/// piping an extracted document into `--desc-md -` would otherwise store
/// document-controlled markup on the board. Turning the raw events into text
/// escapes them on render, so the characters survive and the markup does not.
pub fn to_html(md: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(md, options).map(|event| match event {
        Event::Html(raw) | Event::InlineHtml(raw) => Event::Text(raw),
        other => other,
    });
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

/// Flatten HTML to readable plain text.
///
/// Deliberately minimal: it turns block tags into line breaks, drops the rest
/// of the markup, and decodes the handful of entities Plane emits. The raw
/// field is always one `--json` away when the exact markup matters.
pub fn to_text(html_in: &str) -> String {
    let mut out = String::new();
    let mut chars = html_in.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '<' {
            out.push(c);
            continue;
        }
        let mut tag = String::new();
        for t in chars.by_ref() {
            if t == '>' {
                break;
            }
            tag.push(t);
        }
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        let closing = tag.starts_with('/');
        match name.as_str() {
            "br" => out.push('\n'),
            // Block tags break the line where they end, so text does not run
            // together into one paragraph.
            "p" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote"
            | "pre" | "ul" | "ol" | "table"
                if closing =>
            {
                out.push('\n')
            }
            _ => {}
        }
    }
    decode_entities(&out).trim().to_string()
}

fn decode_entities(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        // `&amp;` last, so `&amp;lt;` does not decode twice into `<`.
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_becomes_html() {
        let html = to_html("**bold** and `code`");
        assert!(html.contains("<strong>bold</strong>"));
        assert!(html.contains("<code>code</code>"));
    }

    #[test]
    fn raw_html_is_escaped_rather_than_passed_through() {
        // A block-level script tag: the characters stay, the tag does not.
        let html = to_html("<script>alert(1)</script>");
        assert!(!html.contains("<script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "{html}"
        );
        // Inline HTML in the middle of a paragraph, same treatment, and the
        // surrounding markdown still renders.
        let inline = to_html("a <img src=x onerror=alert(1)> **b**");
        assert!(!inline.contains("<img"), "{inline}");
        assert!(inline.contains("&lt;img"), "{inline}");
        assert!(inline.contains("<strong>b</strong>"), "{inline}");
    }

    #[test]
    fn html_becomes_readable_text() {
        let html = "<p class=\"editor-paragraph-block\"><strong>Waiting for:</strong> Jonas</p><p>Second line</p>";
        assert_eq!(to_text(html), "Waiting for: Jonas\nSecond line");
    }

    #[test]
    fn line_breaks_survive() {
        assert_eq!(to_text("<p>a<br>b</p>"), "a\nb");
    }

    #[test]
    fn entities_are_decoded_once() {
        assert_eq!(
            to_text("<p>Bau&amp;Co &lt;tag&gt; &quot;q&quot;</p>"),
            "Bau&Co <tag> \"q\""
        );
        // A literal, escaped entity must not decode into a real angle bracket.
        assert_eq!(to_text("<p>&amp;lt;</p>"), "&lt;");
    }

    #[test]
    fn umlauts_and_non_ascii_survive_the_round_trip() {
        let html = to_html("Prüfungsverwaltung, Grünflächen, München");
        assert_eq!(to_text(&html), "Prüfungsverwaltung, Grünflächen, München");
    }
}
