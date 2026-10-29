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
/// Paragraphs are separated by a blank line, list items become `- ` or `1. ` lines
/// indented by nesting depth, task items carry `[ ]` or `[x]`, table cells are
/// joined with ` | `, a link keeps its target in parentheses, and entities are
/// decoded once. Source whitespace collapses the way a browser collapses it,
/// except inside `<pre>`. The rest of the markup is dropped; the raw field is
/// always one `--raw` away when the exact markup matters.
pub fn to_text(html_in: &str) -> String {
    let mut w = TextWriter::default();
    let mut text = String::new();
    let mut chars = html_in.chars();
    while let Some(c) = chars.next() {
        if c != '<' {
            text.push(c);
            continue;
        }
        w.text(&text);
        text.clear();
        // A `>` inside a quoted attribute value does not end the tag.
        let mut tag = String::new();
        let mut quote: Option<char> = None;
        for t in chars.by_ref() {
            match quote {
                Some(q) if t == q => quote = None,
                Some(_) => {}
                None if t == '"' || t == '\'' => quote = Some(t),
                None if t == '>' => break,
                None => {}
            }
            tag.push(t);
        }
        w.tag(&tag);
    }
    w.text(&text);
    w.finish()
}

enum List {
    Bullet,
    Ordered(u64),
}

/// The state [`to_text`] carries from one tag to the next.
#[derive(Default)]
struct TextWriter {
    out: String,
    /// The current line holds text, not just an indent or a list marker.
    line_has_content: bool,
    /// Whitespace was seen since the last character written.
    pending_space: bool,
    lists: Vec<List>,
    pre_depth: usize,
    /// The open `<li>` already printed its checkbox from `data-checked`, so
    /// the `<input>` inside it must not print a second one.
    box_done: bool,
    /// Open links: the target and where their text starts in `out`.
    links: Vec<(String, usize)>,
    table_depth: usize,
}

impl TextWriter {
    fn newline(&mut self) {
        self.out.push('\n');
        self.line_has_content = false;
        self.pending_space = false;
    }

    /// Start a new line unless the current one is still empty.
    fn ensure_line(&mut self) {
        if self.line_has_content {
            self.newline();
        }
        self.pending_space = false;
    }

    fn text(&mut self, raw: &str) {
        if raw.is_empty() {
            return;
        }
        let decoded = decode_entities(raw);
        if self.pre_depth > 0 {
            self.out.push_str(&decoded);
            if let Some(last) = decoded.chars().last() {
                self.line_has_content = last != '\n';
            }
            return;
        }
        for c in decoded.chars() {
            if c.is_whitespace() {
                self.pending_space = true;
                continue;
            }
            if self.pending_space && self.line_has_content {
                self.out.push(' ');
            }
            self.pending_space = false;
            self.out.push(c);
            self.line_has_content = true;
        }
    }

    fn tag(&mut self, tag: &str) {
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .flat_map(char::to_lowercase)
            .collect();
        match (name.as_str(), closing) {
            ("br", _) => self.newline(),
            // Plane wraps every table cell's text in a <p>; breaking the line
            // there would put each cell on a line of its own.
            ("p", _) if self.table_depth > 0 => {}
            ("table", false) => {
                self.block();
                self.table_depth += 1;
            }
            ("table", true) => {
                self.table_depth = self.table_depth.saturating_sub(1);
                self.ensure_line();
            }
            ("ul", false) => {
                self.block();
                self.lists.push(List::Bullet);
            }
            ("ol", false) => {
                self.block();
                let start = attr(tag, "start").and_then(|s| s.parse().ok());
                self.lists.push(List::Ordered(start.unwrap_or(1)));
            }
            ("ul" | "ol", true) => {
                self.lists.pop();
                self.ensure_line();
            }
            ("li", false) => self.list_item(tag),
            ("li", true) => {
                self.box_done = false;
                self.ensure_line();
            }
            ("input", false) if attr(tag, "type").as_deref() == Some("checkbox") => {
                if self.box_done {
                    self.box_done = false;
                } else {
                    let mark = checkbox(attr(tag, "checked").is_some());
                    self.out.push_str(mark);
                }
            }
            ("td" | "th", false) => {
                if self.line_has_content {
                    self.out.push_str(" | ");
                }
                self.pending_space = false;
            }
            ("a", false) => {
                let href = attr(tag, "href").unwrap_or_default();
                self.links.push((href, self.out.len()));
            }
            ("a", true) => {
                if let Some((href, start)) = self.links.pop() {
                    let label = self.out.get(start..).unwrap_or("").trim();
                    let bare = href.trim_start_matches("mailto:");
                    if !href.is_empty() && !href.starts_with('#') && label != bare {
                        self.out.push_str(&format!(" ({href})"));
                        self.line_has_content = true;
                    }
                }
            }
            ("hr", _) => {
                self.block();
                self.out.push_str("---");
                self.newline();
            }
            ("pre", false) => {
                self.block();
                self.pre_depth += 1;
            }
            ("pre", true) => {
                self.pre_depth = self.pre_depth.saturating_sub(1);
                self.line_has_content = !self.out.ends_with('\n');
                self.ensure_line();
            }
            // Paragraph-level blocks open with a blank line, as in markdown,
            // so paragraphs stay paragraphs if the text is written back.
            ("p" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote", false) => self.block(),
            // Everything else that is a block just breaks the line, so text
            // does not run together.
            ("p" | "div" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "blockquote", _) => {
                self.ensure_line()
            }
            _ => {}
        }
    }

    /// Open a paragraph-level block: a blank line after whatever came before,
    /// except inside a list, where a paragraph is just the item's text and a
    /// blank line would split the list in two.
    fn block(&mut self) {
        self.ensure_line();
        if self.lists.is_empty() && !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.newline();
        }
    }

    /// Open a list item: a fresh line, two spaces per level of nesting, then
    /// the marker. Plane's task items carry their state in `data-checked`.
    fn list_item(&mut self, tag: &str) {
        self.ensure_line();
        let depth = self.lists.len().saturating_sub(1);
        let marker = match self.lists.last_mut() {
            Some(List::Ordered(n)) => {
                let m = format!("{n}. ");
                *n += 1;
                m
            }
            Some(List::Bullet) | None => "- ".to_string(),
        };
        self.out.push_str(&"  ".repeat(depth));
        self.out.push_str(&marker);
        self.box_done = false;
        if let Some(state) = attr(tag, "data-checked") {
            self.out.push_str(checkbox(state == "true"));
            self.box_done = true;
        }
    }

    fn finish(self) -> String {
        let mut out = String::new();
        let mut blank_run = 0;
        for line in self.out.lines() {
            let line = line.trim_end();
            if line.is_empty() {
                blank_run += 1;
                if blank_run > 1 {
                    continue;
                }
            } else {
                blank_run = 0;
            }
            out.push_str(line);
            out.push('\n');
        }
        out.trim().to_string()
    }
}

fn checkbox(checked: bool) -> &'static str {
    if checked {
        "[x] "
    } else {
        "[ ] "
    }
}

/// The value of attribute `name` in a raw tag, entities decoded; `Some("")`
/// for a bare attribute such as `checked`.
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag
        .trim_start_matches('/')
        .trim_start_matches(|c: char| c.is_ascii_alphanumeric());
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if rest.is_empty() {
            return None;
        }
        let key_end = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let key = &rest[..key_end];
        rest = rest[key_end..].trim_start();
        let value = match rest.strip_prefix('=') {
            Some(after) => {
                let after = after.trim_start();
                let (v, tail) = match after.chars().next() {
                    Some(q @ ('"' | '\'')) => {
                        let body = &after[1..];
                        let end = body.find(q).unwrap_or(body.len());
                        (&body[..end], body.get(end + 1..).unwrap_or(""))
                    }
                    _ => {
                        let end = after.find(char::is_whitespace).unwrap_or(after.len());
                        (&after[..end], &after[end..])
                    }
                };
                rest = tail;
                v
            }
            None => "",
        };
        if key.eq_ignore_ascii_case(name) {
            return Some(decode_entities(value));
        }
        if key.is_empty() {
            // Not an attribute at all; stop rather than loop on it.
            return None;
        }
    }
}

/// Decode HTML entities in one pass, so `&amp;lt;` becomes `&lt;` and not
/// `<`. Numeric references are decoded in full; named ones cover what Plane's
/// editor and pulldown-cmark emit, and an unknown one is left as written.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        // `&` is one byte, so `rest[1..]` is a char boundary; the longest
        // reference decoded here (`&#x10FFFF;`) is ten characters.
        let decoded = rest[1..]
            .char_indices()
            .take(11)
            .find(|&(_, c)| c == ';')
            .and_then(|(semi, _)| entity(&rest[1..1 + semi]).map(|c| (c, semi + 2)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
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

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
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
        "ndash" => '\u{2013}',
        "mdash" => '\u{2014}',
        "hellip" => '\u{2026}',
        "lsquo" => '\u{2018}',
        "rsquo" => '\u{2019}',
        "ldquo" => '\u{201C}',
        "rdquo" => '\u{201D}',
        "laquo" => '\u{00AB}',
        "raquo" => '\u{00BB}',
        "euro" => '\u{20AC}',
        "copy" => '\u{00A9}',
        "deg" => '\u{00B0}',
        _ => return None,
    })
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
        assert_eq!(to_text(html), "Waiting for: Jonas\n\nSecond line");
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
    fn list_items_become_marked_lines() {
        // Plane's comment markup, as stored on PERSO-230.
        let html = "<p>Checked:</p>\n<ul>\n<li>31,763 files, <code>Pictures</code></li>\n<li>Only 7,300\n on disk</li>\n</ul>\n<p><em>By Jean-Claude.</em></p>";
        assert_eq!(
            to_text(html),
            "Checked:\n\n- 31,763 files, Pictures\n- Only 7,300 on disk\n\nBy Jean-Claude."
        );
    }

    #[test]
    fn ordered_and_nested_lists_keep_numbers_and_depth() {
        let html = "<ol start=\"3\"><li><p>three</p><ul><li><p>sub</p></li></ul></li><li><p>four</p></li></ol>";
        assert_eq!(to_text(html), "3. three\n  - sub\n4. four");
    }

    #[test]
    fn task_items_carry_their_checkbox_once() {
        // Plane's editor: the state on the <li>, and an <input> inside it.
        let plane = "<ul data-type=\"taskList\"><li data-checked=\"true\" data-type=\"taskItem\"><label><input type=\"checkbox\" checked=\"checked\"><span></span></label><div><p>done</p></div></li><li data-checked=\"false\"><label><input type=\"checkbox\"></label><div><p>open</p></div></li></ul>";
        assert_eq!(to_text(plane), "- [x] done\n- [ ] open");
        // pulldown-cmark: only the <input>.
        assert_eq!(to_text(&to_html("- [x] a\n- [ ] b")), "- [x] a\n- [ ] b");
    }

    #[test]
    fn links_keep_their_target_unless_it_is_the_text() {
        assert_eq!(
            to_text("<p>see <a href=\"https://x.org/a?b=1&amp;c=2\">the doc</a>.</p>"),
            "see the doc (https://x.org/a?b=1&c=2)."
        );
        assert_eq!(
            to_text("<p><a href=\"https://x.org\">https://x.org</a></p>"),
            "https://x.org"
        );
    }

    #[test]
    fn numeric_and_typographic_entities_decode() {
        assert_eq!(
            to_text("<p>it&#39;s &#x27;q&#x27; &ndash; &euro;5&nbsp;&hellip; &bogus; a & b</p>"),
            "it's 'q' \u{2013} \u{20AC}5 \u{2026} &bogus; a & b"
        );
        // Multibyte text right after an entity must not trip the decoder.
        assert_eq!(to_text("<p>&amp;ü&lt;ö</p>"), "&ü<ö");
    }

    #[test]
    fn preformatted_text_keeps_its_whitespace() {
        let html = to_html("```\nfn a() {\n    b();\n}\n```\nafter");
        assert_eq!(to_text(&html), "fn a() {\n    b();\n}\n\nafter");
    }

    #[test]
    fn an_empty_paragraph_is_one_blank_line_and_no_more() {
        assert_eq!(to_text("<p>a</p><p></p><p></p><p>b</p>"), "a\n\nb");
    }

    #[test]
    fn a_quoted_angle_bracket_does_not_end_the_tag() {
        assert_eq!(to_text("<p title=\"a>b\">x</p>"), "x");
    }

    #[test]
    fn table_cells_are_joined_on_one_line() {
        let html = to_html("| a | b |\n|---|---|\n| 1 | 2 |");
        assert_eq!(to_text(&html), "a | b\n1 | 2");
        // Plane's editor wraps each cell's text in a paragraph.
        let plane = "<p>before</p><table><tbody><tr><td><p>a</p></td><td><p>b</p></td></tr><tr><td><p>1</p></td><td><p>2</p></td></tr></tbody></table><p>after</p>";
        assert_eq!(to_text(plane), "before\n\na | b\n1 | 2\n\nafter");
    }

    #[test]
    fn umlauts_and_non_ascii_survive_the_round_trip() {
        let html = to_html("Prüfungsverwaltung, Grünflächen, München");
        assert_eq!(to_text(&html), "Prüfungsverwaltung, Grünflächen, München");
    }
}
