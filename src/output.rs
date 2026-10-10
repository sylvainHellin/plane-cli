//! Rendering. Aligned lines by default, compact JSON under `--json`, the raw
//! API body under `--raw`.
//!
//! `--json` is the view a script reads: names resolved, no UUIDs a command
//! does not take back, no editor markup, one array element per line. `--raw`
//! prints the API body as it came back, so anything the compact view leaves
//! out is still one flag away. Either way a list prints as a bare array,
//! every page already merged, so `jq '.[]'` walks it the same way it walks a
//! single object fetched with `get`.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::config::{self, Resolved};
use crate::markdown;

/// How a command prints its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Aligned lines for a person.
    Text,
    /// Compact, resolved JSON for a script.
    Json,
    /// The API body as it came back, pretty-printed.
    Raw,
}

impl Format {
    /// `--raw` wins over `--json`, so `--json --raw` and `--raw` mean the same.
    pub fn from_flags(json: bool, raw: bool) -> Self {
        match (json, raw) {
            (_, true) => Format::Raw,
            (true, false) => Format::Json,
            (false, false) => Format::Text,
        }
    }
}

/// Print the view `format` asks for. `compact` is only built under `--json`,
/// so the rendered and raw paths never pay for it.
pub fn emit(
    format: Format,
    raw: &Value,
    compact: impl FnOnce() -> Value,
    rendered: impl FnOnce() -> String,
) {
    match format {
        Format::Raw => println!(
            "{}",
            serde_json::to_string_pretty(raw).unwrap_or_else(|e| format!("JSON error: {e}"))
        ),
        Format::Json => println!("{}", compact_json(&compact())),
        Format::Text => {
            let text = rendered();
            if !text.is_empty() {
                println!("{text}");
            }
        }
    }
}

/// [`emit`] for a value the CLI built itself, where raw and compact are one
/// and the same: the config commands, and the flat attachment entries.
pub fn emit_own(format: Format, value: &Value, rendered: impl FnOnce() -> String) {
    emit(format, value, || value.clone(), rendered);
}

/// Serialise without indentation, but one array element per line: a list of a
/// hundred issues stays a hundred readable lines rather than one 40 KB line or
/// three thousand indented ones, and it is still a single JSON document.
pub fn compact_json(v: &Value) -> String {
    match v.as_array() {
        Some(items) if !items.is_empty() => {
            let lines: Vec<String> = items.iter().map(Value::to_string).collect();
            format!("[\n{}\n]", lines.join(",\n"))
        }
        _ => v.to_string(),
    }
}

/// A string field as JSON: the string, or `null` when absent, null, or empty,
/// so a script tests one thing for "not set".
fn opt_str(v: &Value, key: &str) -> Value {
    match field(v, key) {
        "" => Value::Null,
        s => json!(s),
    }
}

/// What the compact view of an issue needs beyond the issue itself.
pub struct IssueContext<'a> {
    /// The project identifier, e.g. `RES`, which every reference is built on.
    pub identifier: &'a str,
    pub workspace: &'a str,
    /// Module names by issue UUID. The relation lives in a join table, so the
    /// issue record cannot say which modules hold it.
    pub modules: &'a HashMap<String, Vec<String>>,
}

/// The compact `--json` view of one issue: names instead of UUIDs, a
/// reference instead of a sequence number, and no audit or editor fields.
///
/// `description` is plain text and only present when asked for: `issue get`
/// and the single-issue writes carry it, `issue list` does not, because a list
/// of bodies is what made the raw list forty times its rendered size.
pub fn issue_compact(issue: &Value, ctx: &IssueContext, with_description: bool) -> Value {
    let reference = issue
        .get("sequence_id")
        .and_then(Value::as_i64)
        .map(|n| format!("{}-{n}", ctx.identifier))
        .unwrap_or_default();
    let state = issue.get("state");
    let modules = ctx
        .modules
        .get(field(issue, "id"))
        .cloned()
        .unwrap_or_default();
    let mut out = json!({
        "identifier": reference,
        "name": field(issue, "name"),
        "state": state.map(|s| opt_str(s, "name")).unwrap_or(Value::Null),
        "state_group": state.map(|s| opt_str(s, "group")).unwrap_or(Value::Null),
        "priority": opt_str(issue, "priority"),
        "labels": label_names(issue),
        "modules": modules,
        "assignees": assignee_display_names(issue),
        "start": opt_str(issue, "start_date"),
        "due": opt_str(issue, "target_date"),
        "created_at": opt_str(issue, "created_at"),
        "updated_at": opt_str(issue, "updated_at"),
        "completed_at": opt_str(issue, "completed_at"),
        "parent": parent_ref(issue, ctx.identifier),
        "url": issue_url(ctx.workspace, &reference),
    });
    if with_description {
        out["description"] = json!(markdown::to_text(field(issue, "description_html")));
    }
    out
}

/// The parent of an issue fetched with `?expand=parent`, as a reference.
///
/// The expansion carries the parent's `sequence_id` and turns a null parent
/// into `{}`, so an object without a number is "no parent". CE keeps a
/// parent in its child's project, so the child's identifier is the parent's.
fn parent_ref(issue: &Value, identifier: &str) -> Value {
    match issue.get("parent") {
        Some(Value::Object(p)) => p
            .get("sequence_id")
            .and_then(Value::as_i64)
            .map(|n| json!(format!("{identifier}-{n}")))
            .unwrap_or(Value::Null),
        // Unexpanded: a UUID is still better than claiming there is no parent.
        Some(Value::String(s)) if !s.is_empty() => json!(s),
        _ => Value::Null,
    }
}

/// Assignees by display name, the unique handle `--assignee` also accepts;
/// first name only where a member has no display name.
fn assignee_display_names(issue: &Value) -> Vec<&str> {
    issue
        .get("assignees")
        .and_then(Value::as_array)
        .map(|xs| {
            xs.iter()
                .filter_map(|a| {
                    [field(a, "display_name"), field(a, "first_name")]
                        .into_iter()
                        .find(|s| !s.is_empty())
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A project as `--json` lists it. The UUID stays: a bridged note's
/// `plane_project_id` is the one place a command takes it back.
pub fn project_compact(p: &Value) -> Value {
    json!({"identifier": field(p, "identifier"), "name": field(p, "name"), "id": field(p, "id")})
}

/// A module as `--json` lists it, UUID kept for `plane_module_id`.
pub fn module_compact(m: &Value) -> Value {
    json!({
        "name": field(m, "name"),
        "status": opt_str(m, "status"),
        "start": opt_str(m, "start_date"),
        "due": opt_str(m, "target_date"),
        "id": field(m, "id"),
    })
}

/// A state as `--json` lists it. Every write takes the name.
pub fn state_compact(s: &Value) -> Value {
    json!({"name": field(s, "name"), "group": opt_str(s, "group")})
}

/// A label as `--json` lists it. Every write takes the name.
pub fn label_compact(l: &Value) -> Value {
    json!({"name": field(l, "name"), "color": opt_str(l, "color")})
}

/// A comment as `--json` reports it: which issue, the text, and when.
pub fn comment_compact(comment: &Value, reference: &str) -> Value {
    json!({
        "issue": reference,
        "text": markdown::to_text(field(comment, "comment_html")),
        "created_at": opt_str(comment, "created_at"),
    })
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

/// The people an issue is assigned to, from `?expand=assignees`. First name
/// where there is one, else display name: the first name is what a person
/// recognises, the display name is the fallback identifier.
pub fn assignee_names(issue: &Value) -> Vec<&str> {
    issue
        .get("assignees")
        .and_then(Value::as_array)
        .map(|xs| {
            xs.iter()
                .filter_map(|a| {
                    let first = a.get("first_name").and_then(Value::as_str).unwrap_or("");
                    if !first.is_empty() {
                        Some(first)
                    } else {
                        a.get("display_name").and_then(Value::as_str)
                    }
                })
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
        "  start:    {}\n",
        dash(field(issue, "start_date"))
    ));
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
    let assignees = assignee_names(issue);
    out.push_str(&format!(
        "  assigned: {}\n",
        if assignees.is_empty() {
            "-".to_string()
        } else {
            assignees.join(", ")
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
        .map(|a| {
            let n = attachment_name(a);
            if n.is_empty() { "?" } else { n }.to_string()
        })
        .collect();
    let sizes: Vec<String> = attachments
        .iter()
        .map(|a| human_size(attachment_size(a) as f64))
        .collect();
    let types: Vec<String> = attachments
        .iter()
        .map(|a| dash(attachment_type(a)).to_string())
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
/// Empty when the raw asset carries neither the attributes object nor a name
/// in it; the table prints `?` for that, the JSON prints the empty string.
pub fn attachment_name(attachment: &Value) -> &str {
    attachment
        .get("attributes")
        .map(|at| field(at, "name"))
        .unwrap_or("")
}

/// The MIME type of an attachment, which also lives under `attributes`.
pub fn attachment_type(attachment: &Value) -> &str {
    attachment
        .get("attributes")
        .map(|at| field(at, "type"))
        .unwrap_or("")
}

/// The byte count of an attachment, as an integer.
///
/// CE reports the top-level `size` as a float (`70.0`) and the one under
/// `attributes` as an integer, and a byte count is not a fractional quantity,
/// so both commands emit the integer. A negative or absent value reads as 0
/// rather than panicking on a cast.
pub fn attachment_size(attachment: &Value) -> u64 {
    [
        attachment.get("size"),
        attachment.get("attributes").and_then(|at| at.get("size")),
    ]
    .into_iter()
    .flatten()
    .find_map(|v| {
        v.as_u64()
            .or_else(|| v.as_f64().map(|f| f.max(0.0).round() as u64))
    })
    .unwrap_or(0)
}

/// The flat JSON shape both attachment commands emit: `id`, `name`, `size`,
/// `type`, `asset_url`, in that order.
///
/// `attach` knows all five from the upload it just performed; `attachments`
/// digs them out of Plane's raw asset object through [`attachment_flat`].
/// One constructor, so the two shapes cannot drift apart again.
pub fn attachment_entry(id: &str, name: &str, size: u64, mime: &str, asset_url: Value) -> Value {
    serde_json::json!({
        "id": id,
        "name": name,
        "size": size,
        "type": mime,
        "asset_url": asset_url,
    })
}

/// Flatten one of Plane's raw asset objects into that shape. The raw object
/// carries no `asset_url`, so the caller passes the one it derived.
pub fn attachment_flat(asset: &Value, asset_url: Value) -> Value {
    attachment_entry(
        field(asset, "id"),
        attachment_name(asset),
        attachment_size(asset),
        attachment_type(asset),
        asset_url,
    )
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
        assert!(out.contains("start:    -"));
        assert!(out.contains("due:      -"));
        assert!(out.contains("labels:   -"));
        assert!(out.contains("assigned: -"));
    }

    #[test]
    fn the_start_date_sits_just_above_the_due_date() {
        let issue = json!({
            "name": "T", "sequence_id": 1, "state": {"name": "Todo"},
            "start_date": "2026-08-01", "target_date": "2026-08-15"
        });
        let out = issue_detail(&issue, "RES-1", "acme");
        assert!(
            out.contains("  start:    2026-08-01\n  due:      2026-08-15\n"),
            "{out}"
        );
    }

    #[test]
    fn assignees_render_by_first_name_from_the_expand() {
        let issue = json!({
            "name": "T", "sequence_id": 1, "state": {"name": "Todo"},
            "assignees": [
                {"first_name": "Robin", "display_name": "sylvain.s.personal.assistant"},
                {"first_name": "", "display_name": "plane.sincere"},
            ]
        });
        let out = issue_detail(&issue, "RES-1", "acme");
        assert!(out.contains("assigned: Robin, plane.sincere"), "{out}");
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
    fn attach_and_attachments_emit_the_same_flat_json_shape() {
        let url = "/api/assets/v2/workspaces/acme/projects/p1/issues/i1/attachments/a1/";
        // What `attach --json` builds from the upload it just performed.
        let attached = attachment_entry("a1", "plan.pdf", 32, "application/pdf", json!(url));
        // What `attachments --json` flattens out of Plane's raw asset object,
        // where name and type hide under `attributes` and size is a float.
        let raw = json!({
            "id": "a1", "size": 32.0, "is_uploaded": true, "created_at": "2026-01-01",
            "attributes": {"name": "plan.pdf", "type": "application/pdf", "size": 32},
        });
        let listed = attachment_flat(&raw, json!(url));

        assert_eq!(attached, listed, "the two commands must not drift apart");
        let keys: Vec<&String> = listed.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["asset_url", "id", "name", "size", "type"]);
        // Bytes are an integer on both sides, never CE's float.
        assert!(listed["size"].is_u64(), "{listed}");
        assert_eq!(listed["size"], json!(32));

        // A raw asset carrying neither `attributes` nor a usable size is a
        // malformed row, not a panic.
        let bare = attachment_flat(&json!({"id": "a2", "size": null}), Value::Null);
        assert_eq!(
            bare,
            json!({"id": "a2", "name": "", "size": 0, "type": "", "asset_url": null})
        );
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

    /// An issue as CE returns it under `EXPAND_COMPACT`, audit fields and all.
    fn expanded_issue() -> Value {
        json!({
            "id": "i-uuid", "sequence_id": 12, "name": "Write the paper",
            "project": "p-uuid", "workspace": "w-uuid", "created_by": "u-uuid",
            "priority": "high", "start_date": "2026-08-01", "target_date": null,
            "created_at": "2026-07-30T10:50:15Z", "updated_at": "2026-09-11T15:17:22Z",
            "completed_at": null, "sort_order": 65535.0,
            "state": {"id": "s-uuid", "name": "In Progress", "group": "started", "color": "#f00"},
            "labels": [{"id": "l-uuid", "name": "deep"}],
            "assignees": [
                {"id": "a-uuid", "first_name": "Robin", "display_name": "robin.assistant"},
                {"id": "b-uuid", "first_name": "Sam", "display_name": ""},
            ],
            "parent": {"id": "x-uuid", "sequence_id": 4, "project_id": "p-uuid"},
            "description_html": "<p>First &amp; <strong>second</strong></p><p>Third</p>",
        })
    }

    #[test]
    fn the_compact_issue_names_everything_and_carries_no_uuid() {
        let modules = HashMap::from([("i-uuid".to_string(), vec!["Paper 2".to_string()])]);
        let ctx = IssueContext {
            identifier: "RES",
            workspace: "acme",
            modules: &modules,
        };
        let out = issue_compact(&expanded_issue(), &ctx, true);
        assert_eq!(out["identifier"], json!("RES-12"));
        assert_eq!(out["name"], json!("Write the paper"));
        assert_eq!(out["state"], json!("In Progress"));
        assert_eq!(out["state_group"], json!("started"));
        assert_eq!(out["priority"], json!("high"));
        assert_eq!(out["labels"], json!(["deep"]));
        assert_eq!(out["modules"], json!(["Paper 2"]));
        // Display name is the unique handle; first name only fills a gap.
        assert_eq!(out["assignees"], json!(["robin.assistant", "Sam"]));
        assert_eq!(out["start"], json!("2026-08-01"));
        assert_eq!(out["due"], Value::Null);
        assert_eq!(out["completed_at"], Value::Null);
        assert_eq!(out["parent"], json!("RES-4"));
        assert_eq!(out["description"], json!("First & second\nThird"));
        assert!(
            !out.to_string().contains("uuid"),
            "no UUID may leak into the compact view: {out}"
        );
    }

    #[test]
    fn the_list_view_leaves_the_description_out() {
        let ctx = IssueContext {
            identifier: "RES",
            workspace: "acme",
            modules: &HashMap::new(),
        };
        let out = issue_compact(&expanded_issue(), &ctx, false);
        assert!(out.get("description").is_none(), "{out}");
        // An issue in no module still has the key, as an empty list.
        assert_eq!(out["modules"], json!([]));
        let keys: Vec<&String> = out.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "assignees",
                "completed_at",
                "created_at",
                "due",
                "identifier",
                "labels",
                "modules",
                "name",
                "parent",
                "priority",
                "start",
                "state",
                "state_group",
                "updated_at",
                "url"
            ]
        );
    }

    #[test]
    fn an_expanded_null_parent_reads_as_no_parent() {
        // `?expand=parent` turns a null parent into `{}`.
        assert_eq!(parent_ref(&json!({"parent": {}}), "RES"), Value::Null);
        assert_eq!(parent_ref(&json!({"parent": null}), "RES"), Value::Null);
        assert_eq!(parent_ref(&json!({}), "RES"), Value::Null);
        assert_eq!(
            parent_ref(&json!({"parent": {"sequence_id": 68}}), "RES"),
            json!("RES-68")
        );
    }

    #[test]
    fn compact_json_puts_one_array_element_per_line() {
        let v = json!([{"a": 1}, {"a": 2}]);
        let out = compact_json(&v);
        assert_eq!(out, "[\n{\"a\":1},\n{\"a\":2}\n]");
        // Still one JSON document, and the same one.
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap(), v);
        assert_eq!(compact_json(&json!([])), "[]");
        assert_eq!(compact_json(&json!({"a": [1, 2]})), "{\"a\":[1,2]}");
    }

    #[test]
    fn raw_wins_over_json_and_neither_is_text() {
        assert_eq!(Format::from_flags(false, false), Format::Text);
        assert_eq!(Format::from_flags(true, false), Format::Json);
        assert_eq!(Format::from_flags(false, true), Format::Raw);
        assert_eq!(Format::from_flags(true, true), Format::Raw);
    }

    #[test]
    fn list_entities_keep_a_uuid_only_where_a_note_takes_it_back() {
        let p = json!({"id": "p1", "identifier": "RES", "name": "Research", "network": 2});
        assert_eq!(
            project_compact(&p),
            json!({"identifier": "RES", "name": "Research", "id": "p1"})
        );
        let m = json!({"id": "m1", "name": "Paper", "status": "in-progress",
                       "start_date": "2026-10-09", "target_date": null, "project": "p1"});
        assert_eq!(
            module_compact(&m),
            json!({"name": "Paper", "status": "in-progress", "start": "2026-10-09", "due": null, "id": "m1"})
        );
        let s = json!({"id": "s1", "name": "Done", "group": "completed", "project": "p1"});
        assert_eq!(
            state_compact(&s),
            json!({"name": "Done", "group": "completed"})
        );
        let l = json!({"id": "l1", "name": "deep", "color": "#000", "project": "p1"});
        assert_eq!(label_compact(&l), json!({"name": "deep", "color": "#000"}));
    }

    #[test]
    fn a_comment_reports_its_issue_and_plain_text() {
        let c = json!({"id": "c1", "comment_html": "<p>Done &amp; dusted</p>",
                       "created_at": "2026-10-10T08:00:00Z", "actor": "u1"});
        assert_eq!(
            comment_compact(&c, "RES-12"),
            json!({"issue": "RES-12", "text": "Done & dusted", "created_at": "2026-10-10T08:00:00Z"})
        );
    }

    #[test]
    fn padding_counts_characters_not_bytes() {
        // Byte padding would misalign any row holding an umlaut.
        assert_eq!(pad("Grün", 6).chars().count(), 6);
    }
}
