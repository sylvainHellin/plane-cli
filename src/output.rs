//! Rendering. Compact aligned lines by default, raw API body under `--json`.
//!
//! `--json` prints exactly what Plane answered, so anything this module
//! chooses not to show is still one flag away.

use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::config::{self, Resolved};
use crate::markdown;

/// Print either the raw API value or a rendered view.
pub fn emit(raw: &Value, json: bool, rendered: impl FnOnce() -> String) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(raw).unwrap_or_else(|e| format!("JSON error: {e}"))
        );
    } else {
        let text = rendered();
        if !text.is_empty() {
            println!("{text}");
        }
    }
}

/// A string field, or `""` when absent or null.
pub fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// A string field that must exist, for the ones that become URL segments or
/// payload values. Defaulting those to `""` turns a malformed response into a
/// 404 on `projects//issues/` rather than into a legible error.
pub fn required_field<'a>(v: &'a Value, key: &str, what: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "Plane's response for {what} carries no `{key}`, so there is nothing to address."
            )
        })
}

/// The state name of an issue fetched with `?expand=state`.
///
/// `expand` substitutes the object in place, so this reads `.state.name`.
/// There is no `.state_detail` on CE; looking for one silently yields nothing.
pub fn state_name(issue: &Value) -> &str {
    issue
        .get("state")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
}

/// The label names of an issue fetched with `?expand=labels`.
pub fn label_names(issue: &Value) -> Vec<&str> {
    issue
        .get("labels")
        .and_then(Value::as_array)
        .map(|ls| {
            ls.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default()
}

fn dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

fn width(items: &[String]) -> usize {
    items.iter().map(|s| s.chars().count()).max().unwrap_or(0)
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    format!("{s}{}", " ".repeat(w.saturating_sub(n)))
}

/// Browser URL for an issue, which is what a vault bridge line points at.
/// `None` when no web origin is configured.
pub fn issue_url(workspace: &str, reference: &str) -> Option<String> {
    config::web_base().map(|base| format!("{base}/{workspace}/browse/{reference}/"))
}

/// The effective settings, each with the source it came from. The source
/// column is the point of the command: a variable shadowing the file is
/// otherwise indistinguishable from a file that was never written.
///
/// Rows arrive from `config::effective`, which has already masked `api_key`,
/// so this prints `(set)` and the source without ever holding the token.
pub fn config_show(path: &str, exists: bool, rows: &[Resolved]) -> String {
    let names: Vec<String> = rows.iter().map(|r| r.key.name().to_string()).collect();
    let values: Vec<String> = rows
        .iter()
        .map(|r| r.value.clone().unwrap_or_default())
        .collect();
    let kw = width(&names);
    let vw = width(&values);

    let mut out = format!(
        "{path}{}\n",
        if exists { "" } else { "  (not written yet)" }
    );
    for (i, r) in rows.iter().enumerate() {
        out.push_str(&format!(
            "{}  {}  {}\n",
            pad(&names[i], kw),
            pad(dash(&values[i]), vw),
            r.source_label()
        ));
    }
    out.push_str(
        "\nauth: PLANE_API_KEY, else the stored api_key, else pass-cli. The token itself is never printed.",
    );
    out
}

/// One issue in full.
pub fn issue_detail(issue: &Value, reference: &str, workspace: &str) -> String {
    let mut out = format!("{reference}  {}\n", field(issue, "name"));
    out.push_str(&format!("  state:    {}\n", dash(state_name(issue))));
    out.push_str(&format!("  priority: {}\n", dash(field(issue, "priority"))));
    out.push_str(&format!(
        "  due:      {}\n",
        dash(field(issue, "target_date"))
    ));
    let labels = label_names(issue);
    out.push_str(&format!(
        "  labels:   {}\n",
        if labels.is_empty() {
            "-".to_string()
        } else {
            labels.join(", ")
        }
    ));
    if let Some(url) = issue_url(workspace, reference) {
        out.push_str(&format!("  url:      {url}\n"));
    }

    let body = markdown::to_text(field(issue, "description_html"));
    if !body.is_empty() {
        out.push_str(&format!("\n{body}\n"));
    }
    out.trim_end().to_string()
}

/// A list of issues, one aligned line each.
pub fn issue_list(issues: &[Value], identifier: &str, heading: &str) -> String {
    if issues.is_empty() {
        return format!("{heading}: no issues");
    }
    let refs: Vec<String> = issues
        .iter()
        .map(|i| {
            format!(
                "{identifier}-{}",
                i.get("sequence_id")
                    .and_then(Value::as_i64)
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| "?".into())
            )
        })
        .collect();
    let states: Vec<String> = issues
        .iter()
        .map(|i| dash(state_name(i)).to_string())
        .collect();
    let prios: Vec<String> = issues
        .iter()
        .map(|i| dash(field(i, "priority")).to_string())
        .collect();
    let (rw, sw, pw) = (width(&refs), width(&states), width(&prios));

    let mut out = format!("{heading}: {} issues\n", issues.len());
    for (i, issue) in issues.iter().enumerate() {
        out.push_str(&format!(
            "{}  {}  {}  {}\n",
            pad(&refs[i], rw),
            pad(&states[i], sw),
            pad(&prios[i], pw),
            field(issue, "name")
        ));
    }
    out.trim_end().to_string()
}

pub fn project_list(projects: &[Value]) -> String {
    if projects.is_empty() {
        return "No projects".to_string();
    }
    let idents: Vec<String> = projects
        .iter()
        .map(|p| field(p, "identifier").to_string())
        .collect();
    let names: Vec<String> = projects
        .iter()
        .map(|p| field(p, "name").to_string())
        .collect();
    let (iw, nw) = (width(&idents), width(&names));
    let mut out = format!("{} projects\n", projects.len());
    for (i, p) in projects.iter().enumerate() {
        out.push_str(&format!(
            "{}  {}  {}\n",
            pad(&idents[i], iw),
            pad(&names[i], nw),
            field(p, "id")
        ));
    }
    out.trim_end().to_string()
}

pub fn module_list(modules: &[Value], identifier: &str) -> String {
    if modules.is_empty() {
        return format!("{identifier}: no modules");
    }
    let names: Vec<String> = modules
        .iter()
        .map(|m| field(m, "name").to_string())
        .collect();
    let w = width(&names);
    let mut out = format!("{identifier}: {} modules\n", modules.len());
    for (i, m) in modules.iter().enumerate() {
        let status = field(m, "status");
        out.push_str(&format!(
            "{}  {}  {}\n",
            pad(&names[i], w),
            pad(dash(status), 9),
            field(m, "id")
        ));
    }
    out.trim_end().to_string()
}

pub fn label_list(labels: &[Value], identifier: &str) -> String {
    if labels.is_empty() {
        return format!("{identifier}: no labels");
    }
    let names: Vec<String> = labels
        .iter()
        .map(|l| field(l, "name").to_string())
        .collect();
    let w = width(&names);
    let mut out = format!("{identifier}: {} labels\n", labels.len());
    for (i, l) in labels.iter().enumerate() {
        out.push_str(&format!(
            "{}  {}  {}\n",
            pad(&names[i], w),
            pad(dash(field(l, "color")), 9),
            field(l, "id")
        ));
    }
    out.trim_end().to_string()
}

/// Byte counts as a human reads them. The API reports `size` as a float.
pub fn human_size(bytes: f64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut n = bytes.max(0.0);
    let mut unit = 0;
    while n >= 1024.0 && unit < UNITS.len() - 1 {
        n /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", n.round() as u64)
    } else {
        format!("{n:.1} {}", UNITS[unit])
    }
}

/// The attachments of one issue. Name, size and MIME live under
/// `attributes`; `size` is also a top-level float.
pub fn attachment_list(attachments: &[Value], reference: &str) -> String {
    if attachments.is_empty() {
        return format!("{reference}: no attachments");
    }
    let names: Vec<String> = attachments
        .iter()
        .map(|a| attachment_name(a).to_string())
        .collect();
    let sizes: Vec<String> = attachments
        .iter()
        .map(|a| human_size(a.get("size").and_then(Value::as_f64).unwrap_or(0.0)))
        .collect();
    let types: Vec<String> = attachments
        .iter()
        .map(|a| {
            dash(
                a.get("attributes")
                    .map(|at| field(at, "type"))
                    .unwrap_or(""),
            )
            .to_string()
        })
        .collect();
    let (nw, sw, tw) = (width(&names), width(&sizes), width(&types));

    let mut out = format!("{reference}: {} attachments\n", attachments.len());
    for (i, a) in attachments.iter().enumerate() {
        // An unconfirmed upload is a real state on CE: the row exists and the
        // file does not, so it is marked rather than shown as complete.
        let pending = if a.get("is_uploaded").and_then(Value::as_bool) == Some(false) {
            "  (upload not confirmed)"
        } else {
            ""
        };
        out.push_str(&format!(
            "{}  {}  {}  {}{pending}\n",
            pad(&names[i], nw),
            pad(&sizes[i], sw),
            pad(&types[i], tw),
            field(a, "id")
        ));
    }
    out.trim_end().to_string()
}

/// The stored file name of an attachment, which lives under `attributes`.
pub fn attachment_name(attachment: &Value) -> &str {
    attachment
        .get("attributes")
        .map(|at| field(at, "name"))
        .filter(|n| !n.is_empty())
        .unwrap_or("?")
}

pub fn state_list(states: &[Value], identifier: &str) -> String {
    if states.is_empty() {
        return format!("{identifier}: no states");
    }
    let names: Vec<String> = states
        .iter()
        .map(|s| field(s, "name").to_string())
        .collect();
    let w = width(&names);
    let mut out = format!("{identifier}: {} states\n", states.len());
    for (i, s) in states.iter().enumerate() {
        out.push_str(&format!(
            "{}  {}  {}\n",
            pad(&names[i], w),
            pad(field(s, "group"), 9),
            field(s, "id")
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn state_is_read_in_place_not_from_state_detail() {
        let issue = json!({"state": {"name": "Backlog"}, "state_detail": {"name": "WRONG"}});
        assert_eq!(state_name(&issue), "Backlog");
        // Unexpanded: a bare UUID has no name, and must not be printed as one.
        let raw = json!({"state": "26f751c0-0000-4000-8000-000000000000"});
        assert_eq!(state_name(&raw), "");
    }

    #[test]
    fn a_missing_id_errors_instead_of_becoming_an_empty_url_segment() {
        let issue = json!({"id": "48284b59-0000-4000-8000-000000000000", "project": null});
        assert_eq!(
            required_field(&issue, "id", "the created issue").unwrap(),
            "48284b59-0000-4000-8000-000000000000"
        );
        for missing in ["project", "absent"] {
            let err = required_field(&issue, missing, "the issue")
                .unwrap_err()
                .to_string();
            assert!(err.contains(&format!("carries no `{missing}`")), "{err}");
        }
        // An empty string is as unusable as an absent key.
        assert!(required_field(&json!({"id": ""}), "id", "the issue").is_err());
    }

    #[test]
    fn list_lines_align_on_the_widest_reference() {
        let issues = vec![
            json!({"sequence_id": 5, "name": "Short", "priority": "low", "state": {"name": "Todo"}}),
            json!({"sequence_id": 47, "name": "Long", "priority": "high", "state": {"name": "In Progress"}}),
        ];
        let out = issue_list(&issues, "RES", "RES");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "RES: 2 issues");
        assert!(lines[1].starts_with("RES-5   Todo         low   Short"));
        assert!(lines[2].starts_with("RES-47  In Progress  high  Long"));
    }

    #[test]
    fn empty_fields_render_as_a_dash() {
        let issue =
            json!({"name": "T", "sequence_id": 1, "priority": null, "state": {"name": "Todo"}});
        let out = issue_detail(&issue, "RES-1", "acme");
        assert!(out.contains("priority: -"));
        assert!(out.contains("due:      -"));
        assert!(out.contains("labels:   -"));
    }

    #[test]
    fn sizes_read_as_bytes_until_they_do_not() {
        assert_eq!(human_size(0.0), "0 B");
        assert_eq!(human_size(32.0), "32 B");
        assert_eq!(human_size(1024.0), "1.0 KB");
        assert_eq!(human_size(1536.0), "1.5 KB");
        assert_eq!(human_size(5.0 * 1024.0 * 1024.0), "5.0 MB");
    }

    #[test]
    fn attachment_rows_read_name_and_type_out_of_attributes() {
        let attachments = vec![
            json!({"id": "a1", "size": 32.0, "is_uploaded": true,
                   "attributes": {"name": "plan.pdf", "type": "application/pdf", "size": 32}}),
            json!({"id": "a2", "size": 2048.0, "is_uploaded": false,
                   "attributes": {"name": "half.zip", "type": "application/zip", "size": 2048}}),
        ];
        let out = attachment_list(&attachments, "RES-50");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "RES-50: 2 attachments");
        assert!(
            lines[1].starts_with("plan.pdf  32 B    application/pdf  a1"),
            "{}",
            lines[1]
        );
        assert!(
            lines[2].ends_with("a2  (upload not confirmed)"),
            "{}",
            lines[2]
        );
        assert_eq!(attachment_list(&[], "RES-50"), "RES-50: no attachments");
    }

    #[test]
    fn config_show_names_the_source_of_every_row_and_unset_where_there_is_none() {
        use crate::config::{Key, Resolved, Source};

        let row = |key: Key, value: Option<&str>, source: Source| Resolved {
            key,
            value: value.map(str::to_string),
            source,
        };
        let rows = [
            row(Key::Workspace, Some("acme"), Source::File),
            row(
                Key::ApiBase,
                Some("http://localhost:8090/api/v1"),
                Source::Default,
            ),
            // No default and never set: the row exists, the value does not.
            row(Key::WebBase, None, Source::Default),
            row(Key::PassField, Some("PAT"), Source::Env),
            // As `config::effective` hands it over: masked, source intact.
            row(Key::ApiKey, Some(crate::config::MASK), Source::File),
        ];

        let out = config_show("/tmp/plane/config.toml", true, &rows);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "/tmp/plane/config.toml");
        assert_eq!(lines[1], "workspace   acme                          file");
        assert_eq!(
            lines[2],
            "api_base    http://localhost:8090/api/v1  default"
        );
        assert_eq!(lines[3], "web_base    -                             unset");
        assert_eq!(lines[4], "pass_field  PAT                           env");
        assert_eq!(lines[5], "api_key     (set)                         file");
        assert_eq!(lines[6], "");
        assert!(lines[7].contains("The token itself is never printed."));

        // Whatever a token looks like, no substring of it reaches the table.
        assert!(!out.contains("plane_pat"), "{out}");

        // A file that is not there yet says so, so an all-`default` table
        // does not read as a file that was written and ignored.
        let out = config_show("/tmp/plane/config.toml", false, &rows);
        assert_eq!(
            out.lines().next().unwrap(),
            "/tmp/plane/config.toml  (not written yet)"
        );
    }

    #[test]
    fn padding_counts_characters_not_bytes() {
        // Byte padding would misalign any row holding an umlaut.
        assert_eq!(pad("Grün", 6).chars().count(), 6);
    }
}
