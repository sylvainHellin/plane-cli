//! Plane CE REST client.
//!
//! Every quirk of the CE API that cost time at the shell level is encoded
//! here once, so callers never have to remember it:
//!
//! - trailing slashes are mandatory (`projects` 301s, `projects/` 200s)
//! - there are no `-lite` endpoints; the plain forms are the ones that exist
//! - `GET /workspaces/<ws>/issues/<IDENT>-<n>/` resolves a human identifier in
//!   a single call, so link resolution never walks projects then issues
//! - `?expand=state,labels` substitutes the object *in place*; CE has no
//!   `state_detail` key
//! - list responses are cursor-paginated via `next_cursor` /
//!   `next_page_results`, so nothing here reads page 1 and calls it a day
//! - `/modules/<id>/module-issues/` returns issue objects directly, with no
//!   join wrapper

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

use crate::auth;
use crate::config;

const PAGE_SIZE: u32 = 100;
/// Ceiling on pages per list call. At `PAGE_SIZE` that is 50k records, far
/// past anything this instance holds, so hitting it means the cursor is
/// misbehaving rather than that the list is long.
const MAX_PAGES: usize = 500;

/// Valid `priority` values on an issue.
pub const PRIORITIES: [&str; 5] = ["urgent", "high", "medium", "low", "none"];

/// Upload timeout, generous next to the 30 s the API calls get: this one
/// covers a whole file crossing the wire, not a JSON round trip.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

pub struct Client {
    http: reqwest::blocking::Client,
    /// A second client for the presigned upload, which goes to the object
    /// store rather than to the API. It never carries the PAT, so the token
    /// cannot leak to an origin the API named for us.
    uploader: reqwest::blocking::Client,
    base: String,
    workspace: String,
    key: String,
}

impl Client {
    pub fn new() -> Result<Self> {
        let base = config::api_base()?;
        let workspace = config::workspace()?;
        let http = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            // The PAT travels as a custom `X-API-Key` header, and reqwest
            // only strips `Authorization` and `Cookie` across a redirect,
            // never custom headers. Plane also 301s on a missing trailing
            // slash, so a redirect here is a real shape rather than a
            // hypothetical: follow none, and let `send` explain the 3xx.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to build HTTP client")?;
        let uploader = reqwest::blocking::Client::builder()
            .timeout(UPLOAD_TIMEOUT)
            // Same reasoning as above, plus one more: the upload target comes
            // out of an API response, so a redirect from it would be a second
            // hop to a host nothing here vetted. The verified flow answers
            // 204 directly, so following none costs nothing.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("Failed to build upload HTTP client")?;
        Ok(Client {
            http,
            uploader,
            base,
            workspace,
            key: auth::api_key()?,
        })
    }

    pub fn workspace(&self) -> &str {
        &self.workspace
    }

    /// Build a workspace-scoped URL. `path` is everything after
    /// `/workspaces/<ws>/` and must already end in `/`.
    fn url(&self, path: &str) -> String {
        // A real assert, not `debug_assert`: this compiles into the release
        // binary, and one string comparison per request is nothing next to
        // the 3xx it prevents.
        assert!(
            path.ends_with('/'),
            "Plane CE 301s on paths without a trailing slash: {path}"
        );
        format!("{}/workspaces/{}/{}", self.base, self.workspace, path)
    }

    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value> {
        let resp = req
            .header("X-API-Key", &self.key)
            .send()
            .context("Request to Plane failed. Is the instance reachable?")?;
        let status = resp.status();
        let location = resp
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = resp.text().unwrap_or_default();
        if status.is_redirection() {
            let target = if location.is_empty() {
                String::new()
            } else {
                format!(" to {location}")
            };
            bail!(
                "Plane returned {status} and the redirect was not followed{target}.\nOn CE this is a missing trailing slash: `projects` 301s, `projects/` 200s. Redirects are never followed here, because the PAT rides in a custom header that would travel with them."
            );
        }
        if !status.is_success() {
            let detail = body.trim();
            let detail = if detail.len() > 500 {
                format!("{}...", detail.chars().take(500).collect::<String>())
            } else {
                detail.to_string()
            };
            let hint = match status.as_u16() {
                // 60 requests per minute per PAT.
                429 => "\nThe PAT is rate-limited to 60 requests a minute; wait a minute and retry.".to_string(),
                401 | 403 => format!("\nCheck the PAT (`pass-cli vault list` proves the session), and check the workspace slug (`plane config show`): it is case-sensitive and lowercase, and this request used \"{}\".", self.workspace),
                502 | 503 => "\nPlane takes about 90 seconds to boot after a restart; a 502 right after one is expected.".to_string(),
                _ => String::new(),
            };
            bail!("Plane returned {status}: {detail}{hint}");
        }
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body).context("Plane returned a body that is not JSON")
    }

    fn get(&self, path: &str, params: &[(&str, &str)]) -> Result<Value> {
        self.send(self.http.get(self.url(path)).query(params))
    }

    /// GET every page of a list endpoint, following `next_cursor` while
    /// `next_page_results` is true.
    fn get_all(&self, path: &str, params: &[(&str, &str)]) -> Result<Vec<Value>> {
        let page_size = PAGE_SIZE.to_string();
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_PAGES {
                bail!(
                    "Gave up on {path} after {MAX_PAGES} pages. The cursor is not terminating, so this is a server-side pagination fault rather than a long list."
                );
            }
            let mut q: Vec<(&str, &str)> = params.to_vec();
            q.push(("per_page", &page_size));
            if let Some(c) = &cursor {
                q.push(("cursor", c));
            }
            let page = self.send(self.http.get(self.url(path)).query(&q))?;
            match page.get("results").and_then(Value::as_array) {
                Some(results) => all.extend(results.iter().cloned()),
                // Some endpoints answer with a bare array rather than an
                // envelope; take it as a single complete page.
                None => match page.as_array() {
                    Some(items) => {
                        if let Some(warning) = bare_array_warning(path, items.len()) {
                            eprintln!("{warning}");
                        }
                        all.extend(items.iter().cloned());
                        break;
                    }
                    None => bail!("Expected a list response from {path}"),
                },
            }
            match next_cursor(&page, cursor.as_deref())? {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        Ok(all)
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.http.post(self.url(path)).json(body))
    }

    fn patch(&self, path: &str, body: &Value) -> Result<Value> {
        self.send(self.http.patch(self.url(path)).json(body))
    }

    // ---- reads ----

    /// Resolve `RES-12` in one call, with state and labels expanded in place.
    pub fn issue_by_ref(&self, r: &IssueRef) -> Result<Value> {
        self.get(
            &format!("issues/{}/", r.as_str()),
            &[("expand", "state,labels")],
        )
        .with_context(|| format!("Could not resolve issue {}", r.as_str()))
    }

    pub fn projects(&self) -> Result<Vec<Value>> {
        self.get_all("projects/", &[])
    }

    /// Look up a project by its identifier (`RES`), case-insensitively.
    pub fn project_by_identifier(&self, identifier: &str) -> Result<Value> {
        let wanted = identifier.trim().to_uppercase();
        let projects = self.projects()?;
        projects
            .into_iter()
            .find(|p| {
                p.get("identifier")
                    .and_then(Value::as_str)
                    .is_some_and(|i| i.eq_ignore_ascii_case(&wanted))
            })
            .ok_or_else(|| {
                anyhow!("No project with identifier {wanted}. Try `plane project list`.")
            })
    }

    pub fn states(&self, project_id: &str) -> Result<Vec<Value>> {
        self.get_all(&format!("projects/{project_id}/states/"), &[])
    }

    pub fn modules(&self, project_id: &str) -> Result<Vec<Value>> {
        self.get_all(&format!("projects/{project_id}/modules/"), &[])
    }

    /// Labels are per project: the same four names exist once per board, as
    /// separate UUIDs.
    pub fn labels(&self, project_id: &str) -> Result<Vec<Value>> {
        self.get_all(&format!("projects/{project_id}/labels/"), &[])
    }

    /// Attachments live under `work-items/`, not `issues/`. That asymmetry is
    /// real on CE: the `issues/` spelling 404s.
    pub fn attachments(&self, project_id: &str, issue_id: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!("projects/{project_id}/work-items/{issue_id}/attachments/"),
            &[],
        )
    }

    pub fn issues(&self, project_id: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!("projects/{project_id}/issues/"),
            &[("expand", "state,labels")],
        )
    }

    /// Issues of a module. CE returns the issue objects themselves here, so
    /// there is nothing to unwrap.
    pub fn module_issues(&self, project_id: &str, module_id: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!("projects/{project_id}/modules/{module_id}/module-issues/"),
            &[("expand", "state,labels")],
        )
    }

    // ---- writes ----

    pub fn create_issue(&self, project_id: &str, body: &Value) -> Result<Value> {
        self.post(&format!("projects/{project_id}/issues/"), body)
    }

    pub fn update_issue(&self, project_id: &str, issue_id: &str, body: &Value) -> Result<Value> {
        self.patch(&format!("projects/{project_id}/issues/{issue_id}/"), body)
    }

    /// Attach an issue to a module. This is a separate call from creation:
    /// `POST /issues/` has no module field. Verification belongs on the module
    /// side, because the issue record keeps reporting `"module": null`.
    pub fn add_issue_to_module(
        &self,
        project_id: &str,
        module_id: &str,
        issue_id: &str,
    ) -> Result<Value> {
        self.post(
            &format!("projects/{project_id}/modules/{module_id}/module-issues/"),
            &json!({ "issues": [issue_id] }),
        )
    }

    // ---- attachments ----

    /// Step one of the upload: ask for a presigned target. Returns the whole
    /// response, which carries `upload_data`, `asset_id` and `asset_url`.
    pub fn request_attachment(
        &self,
        project_id: &str,
        issue_id: &str,
        name: &str,
        size: u64,
        mime: &str,
    ) -> Result<Value> {
        self.post(
            &format!("projects/{project_id}/work-items/{issue_id}/attachments/"),
            &json!({ "name": name, "size": size, "type": mime }),
        )
    }

    /// Step two: POST the file to the presigned URL as multipart form data,
    /// every returned field first and `file` last, because S3 ignores fields
    /// that arrive after the file part.
    ///
    /// No `X-API-Key` here. The target is a different service from the API,
    /// and the PAT has no business travelling to it.
    pub fn upload_attachment(&self, upload_data: &Value, path: &Path, mime: &str) -> Result<()> {
        let url = upload_data
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Plane's upload response carries no `upload_data.url`."))?;
        let fields = upload_data
            .get("fields")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("Plane's upload response carries no `upload_data.fields`."))?;

        let mut form = reqwest::blocking::multipart::Form::new();
        for (k, v) in fields {
            let v = v.as_str().ok_or_else(|| {
                anyhow!("Plane's presigned field `{k}` is not a string, so it cannot be posted.")
            })?;
            form = form.text(k.clone(), v.to_string());
        }
        // Last, and streamed from disk rather than read into memory.
        let part = reqwest::blocking::multipart::Part::file(path)
            .with_context(|| format!("Could not open {} for upload", path.display()))?
            .mime_str(mime)
            .with_context(|| format!("Invalid MIME type \"{mime}\""))?;
        form = form.part("file", part);

        let resp = self
            .uploader
            .post(url)
            .multipart(form)
            .send()
            .with_context(|| format!("Upload to {url} failed"))?;
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        // The object store answers XML on failure. Truncate it, and never
        // reach for the file's own bytes: an error about an upload is not a
        // place to print what was being uploaded.
        let detail = resp.text().unwrap_or_default();
        let detail = detail.trim().chars().take(500).collect::<String>();
        bail!(
            "The presigned upload to {url} returned {status}: {detail}\nA presigned target expires within the hour and pins the exact byte count, so a file that changed between the request and the upload fails here."
        );
    }

    /// Step three: confirm. Until this lands the attachment exists but stays
    /// invisible, with `is_uploaded: false`.
    pub fn confirm_attachment(
        &self,
        project_id: &str,
        issue_id: &str,
        asset_id: &str,
    ) -> Result<Value> {
        self.patch(
            &format!("projects/{project_id}/work-items/{issue_id}/attachments/{asset_id}/"),
            &json!({}),
        )
    }

    pub fn create_comment(
        &self,
        project_id: &str,
        issue_id: &str,
        comment_html: &str,
    ) -> Result<Value> {
        self.post(
            &format!("projects/{project_id}/issues/{issue_id}/comments/"),
            &json!({ "comment_html": comment_html }),
        )
    }

    // ---- name to UUID resolution ----

    /// Find a state by name, case- and separator-insensitively, so `done`,
    /// `in-progress`, and `In Progress` all land.
    ///
    /// Every project on this instance ships the same five states (Backlog,
    /// Todo, In Progress, Done, Cancelled), but they are read live rather than
    /// hardcoded: a wrong name has to fail loudly here instead of turning into
    /// a filter that quietly matches nothing.
    pub fn find_state(&self, project_id: &str, name: &str) -> Result<Value> {
        let states = self.states(project_id)?;
        find_by_name(&states, name)
            .cloned()
            .ok_or_else(|| no_such_name("state", name, &states))
    }

    /// Resolve a state name to the UUID the API writes.
    pub fn resolve_state(&self, project_id: &str, name: &str) -> Result<String> {
        let state = self.find_state(project_id, name)?;
        state
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("State \"{name}\" has no id"))
    }

    /// Find a module by name, case- and separator-insensitively.
    pub fn find_module(&self, project_id: &str, name: &str) -> Result<Value> {
        let modules = self.modules(project_id)?;
        find_by_name(&modules, name)
            .cloned()
            .ok_or_else(|| no_such_name("module", name, &modules))
    }

    /// Resolve a module name to the UUID the API writes.
    pub fn resolve_module(&self, project_id: &str, name: &str) -> Result<String> {
        let module = self.find_module(project_id, name)?;
        module
            .get("id")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| anyhow!("Module \"{name}\" has no id"))
    }

    /// Find a label by name, case- and separator-insensitively.
    pub fn find_label(&self, project_id: &str, name: &str) -> Result<Value> {
        let labels = self.labels(project_id)?;
        find_by_name(&labels, name)
            .cloned()
            .ok_or_else(|| no_such_name("label", name, &labels))
    }

    /// Resolve several label names in a single fetch, since a write carries
    /// the whole set. Returns the matched label objects, deduplicated, in the
    /// order they were asked for.
    pub fn find_labels(&self, project_id: &str, names: &[String]) -> Result<Vec<Value>> {
        let labels = self.labels(project_id)?;
        resolve_all(&labels, names, "label")
    }
}

/// Match every requested name against `items`, erroring on the first miss.
///
/// Split out of `Client` so the typo path is testable without a server: a
/// name that matches nothing has to fail loudly, because a label silently
/// dropped from a write is indistinguishable from one that was never asked
/// for.
fn resolve_all(items: &[Value], names: &[String], kind: &str) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = Vec::new();
    for name in names {
        let found = find_by_name(items, name)
            .cloned()
            .ok_or_else(|| no_such_name(kind, name, items))?;
        let id = found.get("id").and_then(Value::as_str).unwrap_or_default();
        let already = out
            .iter()
            .any(|v| v.get("id").and_then(Value::as_str).unwrap_or_default() == id);
        if !already {
            out.push(found);
        }
    }
    Ok(out)
}

/// The one error message for a name that matched nothing, listing what the
/// project really has.
fn no_such_name(kind: &str, name: &str, items: &[Value]) -> anyhow::Error {
    let available: Vec<&str> = items
        .iter()
        .filter_map(|i| i.get("name").and_then(Value::as_str))
        .collect();
    let available = if available.is_empty() {
        "none".to_string()
    } else {
        available.join(", ")
    };
    anyhow!("No {kind} named \"{name}\" in this project. Available: {available}")
}

/// Decide the cursor for the next page, or `None` when the list is done.
///
/// Split out of the loop so the stuck-cursor guard is testable without a
/// server: a cursor that repeats means the list is not advancing, and
/// following it would spin until the 60-requests-a-minute limit turned it
/// into a 429, which is a slow and confusing failure rather than a safe one.
fn next_cursor(page: &Value, current: Option<&str>) -> Result<Option<String>> {
    if !page
        .get("next_page_results")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Ok(None);
    }
    let Some(next) = page.get("next_cursor").and_then(Value::as_str) else {
        return Ok(None);
    };
    if Some(next) == current {
        bail!(
            "Plane repeated the pagination cursor \"{next}\", so the list is not advancing. Stopping instead of fetching the same page until the rate limit turns it into a 429."
        );
    }
    Ok(Some(next.to_string()))
}

/// Warn when an unenveloped response is exactly a page long.
///
/// A bare array carries no `next_page_results`, so a full page of them is
/// indistinguishable from a server-side cap that silently dropped the rest.
/// That is the same truncation the plan faults in `aaronshaf/plane-cli`, so
/// it is said out loud rather than assumed away.
fn bare_array_warning(path: &str, len: usize) -> Option<String> {
    (len >= PAGE_SIZE as usize).then(|| {
        format!(
            "Warning: {path} answered with a bare array of {len} items and no pagination envelope, which is the full page size. The result may be truncated."
        )
    })
}

/// Match on `name`, ignoring case, spaces, hyphens and underscores.
fn find_by_name<'a>(items: &'a [Value], name: &str) -> Option<&'a Value> {
    let wanted = normalize(name);
    items.iter().find(|i| {
        i.get("name")
            .and_then(Value::as_str)
            .is_some_and(|n| normalize(n) == wanted)
    })
}

pub fn normalize(s: &str) -> String {
    s.chars()
        .filter(|c| !matches!(c, ' ' | '-' | '_'))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Validate a priority against the values CE accepts.
pub fn normalize_priority(p: &str) -> Result<String> {
    let lower = p.trim().to_lowercase();
    if PRIORITIES.contains(&lower.as_str()) {
        Ok(lower)
    } else {
        bail!(
            "Invalid priority \"{p}\". Use one of: {}",
            PRIORITIES.join(", ")
        )
    }
}

/// Validate a `YYYY-MM-DD` date, which is the only form CE takes for
/// `target_date` / `start_date`.
pub fn validate_date(d: &str) -> Result<String> {
    let d = d.trim();
    let parts: Vec<&str> = d.split('-').collect();
    let shaped = parts.len() == 3
        && parts[0].len() == 4
        && parts[1].len() == 2
        && parts[2].len() == 2
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()));
    if shaped {
        Ok(d.to_string())
    } else {
        bail!("Invalid date \"{d}\". Use YYYY-MM-DD.")
    }
}

/// A human issue reference such as `RES-12`.
#[derive(Debug, Clone)]
pub struct IssueRef {
    text: String,
}

impl IssueRef {
    pub fn parse(s: &str) -> Result<Self> {
        let s = s.trim();
        let (ident, num) = s
            .rsplit_once('-')
            .ok_or_else(|| anyhow!("Invalid issue reference \"{s}\". Expected e.g. RES-12."))?;
        if ident.is_empty()
            || !ident.chars().all(|c| c.is_ascii_alphanumeric())
            || num.is_empty()
            || !num.chars().all(|c| c.is_ascii_digit())
        {
            bail!("Invalid issue reference \"{s}\". Expected e.g. RES-12.");
        }
        Ok(IssueRef {
            text: format!("{}-{}", ident.to_uppercase(), num),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_ref_uppercases_the_identifier() {
        assert_eq!(IssueRef::parse("res-12").unwrap().as_str(), "RES-12");
        assert_eq!(IssueRef::parse(" RES-12 ").unwrap().as_str(), "RES-12");
    }

    #[test]
    fn issue_ref_rejects_junk() {
        for bad in ["RES", "RES-", "-12", "RES-12a", "RES 12", ""] {
            assert!(IssueRef::parse(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn normalize_folds_case_and_separators() {
        assert_eq!(normalize("In Progress"), "inprogress");
        assert_eq!(normalize("in-progress"), "inprogress");
        assert_eq!(normalize("Paper 2 Drawings"), "paper2drawings");
    }

    #[test]
    fn priority_is_validated() {
        assert_eq!(normalize_priority("HIGH").unwrap(), "high");
        assert!(normalize_priority("important").is_err());
    }

    #[test]
    fn a_repeated_cursor_is_refused_rather_than_followed() {
        let page = json!({"next_page_results": true, "next_cursor": "100:0:0"});
        // First sight of the cursor: follow it.
        assert_eq!(
            next_cursor(&page, None).unwrap().as_deref(),
            Some("100:0:0")
        );
        // Same cursor again: the list is not advancing.
        let err = next_cursor(&page, Some("100:0:0")).unwrap_err().to_string();
        assert!(err.contains("repeated the pagination cursor"), "{err}");
    }

    #[test]
    fn pagination_stops_when_the_envelope_says_so() {
        let last = json!({"next_page_results": false, "next_cursor": "100:1:0"});
        assert_eq!(next_cursor(&last, None).unwrap(), None);
        // `next_page_results` true but no cursor to follow is also the end.
        let truncated = json!({"next_page_results": true});
        assert_eq!(next_cursor(&truncated, None).unwrap(), None);
    }

    #[test]
    fn a_full_page_bare_array_warns_about_truncation() {
        assert!(bare_array_warning("projects/", PAGE_SIZE as usize - 1).is_none());
        let warning = bare_array_warning("projects/", PAGE_SIZE as usize).unwrap();
        assert!(warning.contains("may be truncated"), "{warning}");
    }

    #[test]
    fn labels_resolve_by_name_and_deduplicate() {
        let labels = json!([
            {"id": "aaa", "name": "waiting"},
            {"id": "bbb", "name": "deep"},
            {"id": "ccc", "name": "quick"}
        ]);
        let labels = labels.as_array().unwrap();
        let names = [
            "WAITING".to_string(),
            "deep".to_string(),
            "waiting".to_string(),
        ];
        let found = resolve_all(labels, &names, "label").unwrap();
        let ids: Vec<&str> = found
            .iter()
            .map(|l| l.get("id").unwrap().as_str().unwrap())
            .collect();
        // Asked-for order kept, the repeat folded away: the API takes the
        // whole set, so a duplicate would otherwise ride along in the payload.
        assert_eq!(ids, vec!["aaa", "bbb"]);
    }

    #[test]
    fn a_mistyped_label_errors_with_the_real_list() {
        let labels = json!([{"id": "aaa", "name": "waiting"}, {"id": "bbb", "name": "deep"}]);
        let err = resolve_all(labels.as_array().unwrap(), &["wating".to_string()], "label")
            .unwrap_err()
            .to_string();
        assert!(err.contains("No label named \"wating\""), "{err}");
        assert!(err.contains("waiting, deep"), "{err}");
        // A project with no labels at all still says something usable.
        let empty = no_such_name("label", "deep", &[]).to_string();
        assert!(empty.contains("Available: none"), "{empty}");
    }

    #[test]
    fn date_shape_is_validated() {
        assert_eq!(validate_date("2026-08-15").unwrap(), "2026-08-15");
        for bad in ["2026-8-15", "15-08-2026", "tomorrow", "2026-08-15T00:00"] {
            assert!(validate_date(bad).is_err(), "should reject {bad}");
        }
    }
}
