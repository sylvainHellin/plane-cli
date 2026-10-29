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

/// What an issue read expands in place. The rendered and `--raw` views keep
/// the expansions they always had, so `--raw` prints the body it always
/// printed; the compact `--json` view also expands `parent`, which turns the
/// parent's UUID into its `sequence_id` (and a null parent into `{}`), and
/// `assignees`, so it can name both without a call per row.
pub const EXPAND_ISSUE: &str = "state,labels,assignees";
pub const EXPAND_LIST: &str = "state,labels";
pub const EXPAND_COMPACT: &str = "state,labels,assignees,parent";

/// Valid `priority` values on an issue.
pub const PRIORITIES: [&str; 5] = ["urgent", "high", "medium", "low", "none"];

/// Upload timeout, generous next to the 30 s the API calls get: this one
/// covers a whole file crossing the wire, not a JSON round trip.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Backoff between retries of a 429 that carries no usable `Retry-After`.
/// They sum to 62 s, so the last retry lands after the 60 s window has rolled.
const RATE_LIMIT_BACKOFF: [u64; 5] = [2, 4, 8, 16, 32];
/// Ceiling on the total time spent waiting out 429s for one request. A
/// `Retry-After` that would push past it is not honoured; the request fails
/// instead of pausing the caller longer than one rate-limit window.
const RATE_LIMIT_BUDGET: Duration = Duration::from_secs(65);

pub struct Client {
    http: reqwest::blocking::Client,
    /// A second client for the presigned upload, which goes to the object
    /// store rather than to the API. It never carries the PAT, so the token
    /// cannot leak to an origin the API named for us.
    uploader: reqwest::blocking::Client,
    base: String,
    workspace: String,
    key: String,
    /// How a rate-limit wait is spent. `thread::sleep` in the binary; tests
    /// swap in a no-op so the retry loop runs without waiting.
    sleep: fn(Duration),
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
            sleep: std::thread::sleep,
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

    /// Send a request, waiting out 429s transparently.
    ///
    /// A 429 means Plane rejected the request without acting on it, so a
    /// retry is safe for writes too. Nothing else is retried. Each wait
    /// prints one line to stderr, keeping stdout clean for `--json`.
    fn send(&self, req: reqwest::blocking::RequestBuilder) -> Result<Value> {
        let req = req.header("X-API-Key", &self.key);
        let mut waited = Duration::ZERO;
        let mut retries = 0usize;
        loop {
            // Every body sent through here is JSON or empty, so the clone
            // always succeeds; a streaming body would just go out once.
            let Some(attempt) = req.try_clone() else {
                return self.read(req.send());
            };
            let resp = attempt.send();
            if let Ok(r) = &resp {
                if r.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                    let retry_after = r
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|v| v.to_str().ok());
                    if let Some(delay) = rate_limit_delay(retries, retry_after, waited) {
                        eprintln!("plane: rate limited, retrying in {}s", delay.as_secs());
                        (self.sleep)(delay);
                        waited += delay;
                        retries += 1;
                        continue;
                    }
                }
            }
            return self.read(resp);
        }
    }

    /// Turn one response into the parsed body or an explained error.
    fn read(&self, resp: reqwest::Result<reqwest::blocking::Response>) -> Result<Value> {
        let resp = resp.context("Request to Plane failed. Is the instance reachable?")?;
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
        self.issue_by_ref_expanding(r, EXPAND_ISSUE)
    }

    /// [`Self::issue_by_ref`] with a chosen expansion, for the compact view.
    pub fn issue_by_ref_expanding(&self, r: &IssueRef, expand: &str) -> Result<Value> {
        self.get(&format!("issues/{}/", r.as_str()), &[("expand", expand)])
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

    /// The `asset_url` of a stored attachment.
    ///
    /// A listed asset carries no `asset_url` of its own, while the upload
    /// request in step one hands one back, so this rebuilds that exact path
    /// from the ids the listing does carry. It is the asset's address, not a
    /// download link: the endpoint authenticates by browser session and
    /// answers 401 to a PAT.
    pub fn asset_url(&self, project_id: &str, issue_id: &str, asset_id: &str) -> String {
        format!(
            "/api/assets/v2/workspaces/{}/projects/{project_id}/issues/{issue_id}/attachments/{asset_id}/",
            self.workspace
        )
    }

    pub fn issues(&self, project_id: &str, expand: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!("projects/{project_id}/issues/"),
            &[("expand", expand)],
        )
    }

    /// Issues of a module. CE returns the issue objects themselves here, so
    /// there is nothing to unwrap.
    pub fn module_issues(
        &self,
        project_id: &str,
        module_id: &str,
        expand: &str,
    ) -> Result<Vec<Value>> {
        self.get_all(
            &format!("projects/{project_id}/modules/{module_id}/module-issues/"),
            &[("expand", expand)],
        )
    }

    /// The issue UUIDs of a module, and nothing else: `fields=id` trims each
    /// row to its id, so naming the modules of every issue costs one small
    /// call per module rather than a full issue body per row.
    pub fn module_issue_ids(&self, project_id: &str, module_id: &str) -> Result<Vec<String>> {
        Ok(self
            .get_all(
                &format!("projects/{project_id}/modules/{module_id}/module-issues/"),
                &[("fields", "id")],
            )?
            .iter()
            .filter_map(|i| i.get("id").and_then(Value::as_str).map(String::from))
            .collect())
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

    /// Every comment of an issue, every page merged, with `actor` expanded in
    /// place so the author's name comes with it. The order is whatever Plane
    /// returns; callers sort.
    pub fn comments(&self, project_id: &str, issue_id: &str) -> Result<Vec<Value>> {
        self.get_all(
            &format!("projects/{project_id}/issues/{issue_id}/comments/"),
            &[("expand", "actor")],
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

    /// The people who can be assigned an issue: a project's members. Plane
    /// only offers project members in an assignee dropdown, and the API only
    /// accepts their UUIDs in `assignees`, so a workspace member who was never
    /// added to the project is not assignable until they are.
    ///
    /// This endpoint answers a bare array of user objects (`id`, `first_name`,
    /// `display_name`, `email`), not the envelope the issue lists use.
    pub fn members(&self, project_id: &str) -> Result<Vec<Value>> {
        self.get_all(&format!("projects/{project_id}/members/"), &[])
    }

    /// Resolve people to their member UUIDs in a single fetch, since the
    /// `assignees` write carries the whole set. Deduplicated, in the order
    /// asked for. A person is matched by display name, email, first name, or
    /// "First Last", case-insensitively: exact first, then a unique substring.
    pub fn find_members(&self, project_id: &str, names: &[String]) -> Result<Vec<Value>> {
        let members = self.members(project_id)?;
        let mut out: Vec<Value> = Vec::new();
        for name in names {
            let found = find_member(&members, name)?.clone();
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

/// The strings a person can be named by: display name, email, first name,
/// and "First Last". Empty ones are dropped so a member with no last name
/// never matches the empty query.
fn member_labels(m: &Value) -> Vec<String> {
    let g = |k: &str| {
        m.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let first = g("first_name");
    let last = g("last_name");
    let full = format!("{first} {last}").trim().to_string();
    let mut v = vec![g("display_name"), g("email"), first.clone()];
    if !full.is_empty() && full != first {
        v.push(full);
    }
    v.into_iter().filter(|s| !s.is_empty()).collect()
}

/// How a project member reads in an error: `First (display_name)`.
fn member_desc(m: &Value) -> String {
    let g = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").trim();
    let first = g("first_name");
    let display = g("display_name");
    match (first.is_empty(), display.is_empty()) {
        (false, false) => format!("{first} ({display})"),
        (false, true) => first.to_string(),
        (true, false) => display.to_string(),
        (true, true) => g("email").to_string(),
    }
}

/// Match one person against a project's members: exact on any of their names
/// first, then a unique case-insensitive substring. Ambiguity and no-match
/// both fail loudly with the roster, because a silently wrong assignee is
/// worse than a stop.
fn find_member<'a>(members: &'a [Value], name: &str) -> Result<&'a Value> {
    let wanted = name.trim().to_lowercase();
    if wanted.is_empty() {
        bail!("An empty assignee name matches no one.");
    }
    let roster = || {
        let names: Vec<String> = members.iter().map(member_desc).collect();
        if names.is_empty() {
            "none".to_string()
        } else {
            names.join(", ")
        }
    };

    let exact: Vec<&Value> = members
        .iter()
        .filter(|m| member_labels(m).iter().any(|l| l.to_lowercase() == wanted))
        .collect();
    match exact.len() {
        1 => return Ok(exact[0]),
        n if n > 1 => bail!(
            "\"{name}\" matches {n} project members exactly. Use a fuller name. Members: {}",
            roster()
        ),
        _ => {}
    }

    let sub: Vec<&Value> = members
        .iter()
        .filter(|m| {
            member_labels(m)
                .iter()
                .any(|l| l.to_lowercase().contains(&wanted))
        })
        .collect();
    match sub.len() {
        1 => Ok(sub[0]),
        0 => bail!(
            "No project member matches \"{name}\". A workspace member has to be added to the project before they can be assigned. Members: {}",
            roster()
        ),
        n => bail!(
            "\"{name}\" matches {n} project members ({}). Be more specific.",
            sub.iter().map(|m| member_desc(m)).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// How long to wait before retrying a 429, or `None` to give up.
///
/// `retries` is how many retries already happened and `waited` how long they
/// slept in total. A `Retry-After` in whole seconds wins over the backoff
/// table (an HTTP-date one is ignored, since Plane sends seconds); either way
/// the total stays within [`RATE_LIMIT_BUDGET`]. Pure, so the policy is
/// tested without sleeping.
fn rate_limit_delay(
    retries: usize,
    retry_after: Option<&str>,
    waited: Duration,
) -> Option<Duration> {
    let backoff = *RATE_LIMIT_BACKOFF.get(retries)?;
    let secs = retry_after
        .and_then(|v| v.trim().parse::<u64>().ok())
        // `Retry-After: 0` would retry into the same window at once.
        .map(|s| s.max(1))
        .unwrap_or(backoff);
    let delay = Duration::from_secs(secs);
    (waited + delay <= RATE_LIMIT_BUDGET).then_some(delay)
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

    /// The project identifier part, `RES` for `RES-12`.
    pub fn identifier(&self) -> &str {
        self.text.rsplit_once('-').map_or("", |(i, _)| i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    #[test]
    fn rate_limit_backs_off_then_gives_up_within_the_budget() {
        let mut waited = Duration::ZERO;
        let mut delays = Vec::new();
        let mut retries = 0;
        while let Some(d) = rate_limit_delay(retries, None, waited) {
            delays.push(d.as_secs());
            waited += d;
            retries += 1;
        }
        assert_eq!(delays, [2, 4, 8, 16, 32]);
        assert!(waited <= RATE_LIMIT_BUDGET);
    }

    #[test]
    fn rate_limit_honours_retry_after_seconds() {
        assert_eq!(
            rate_limit_delay(0, Some("10"), Duration::ZERO),
            Some(secs(10))
        );
        assert_eq!(rate_limit_delay(3, Some(" 7 "), secs(14)), Some(secs(7)));
        // Zero would retry into the same window at once.
        assert_eq!(
            rate_limit_delay(0, Some("0"), Duration::ZERO),
            Some(secs(1))
        );
    }

    #[test]
    fn rate_limit_falls_back_to_backoff_on_an_unparsable_retry_after() {
        let date = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(rate_limit_delay(2, Some(date), secs(6)), Some(secs(8)));
        assert_eq!(rate_limit_delay(0, Some(""), Duration::ZERO), Some(secs(2)));
    }

    #[test]
    fn rate_limit_refuses_a_retry_after_past_the_budget() {
        assert_eq!(rate_limit_delay(0, Some("120"), Duration::ZERO), None);
        assert_eq!(rate_limit_delay(1, Some("60"), secs(60)), None);
        assert_eq!(rate_limit_delay(1, Some("5"), secs(60)), Some(secs(5)));
    }

    thread_local! {
        static SLEPT: RefCell<Vec<Duration>> = const { RefCell::new(Vec::new()) };
    }

    fn record_sleep(d: Duration) {
        SLEPT.with(|s| s.borrow_mut().push(d));
    }

    fn slept() -> Vec<u64> {
        SLEPT.with(|s| s.borrow().iter().map(Duration::as_secs).collect())
    }

    /// Serve one canned HTTP response per connection, in order, and hand back
    /// what each request was: its request line and its body.
    fn serve(responses: Vec<String>) -> (String, std::thread::JoinHandle<Vec<(String, String)>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
                reader.read_line(&mut request_line).unwrap();
                let mut length = 0usize;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                    if let Some((k, v)) = line.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            length = v.trim().parse().unwrap();
                        }
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                seen.push((
                    request_line.trim().to_string(),
                    String::from_utf8(body).unwrap(),
                ));
                stream.write_all(response.as_bytes()).unwrap();
            }
            seen
        });
        (base, handle)
    }

    fn response(status: &str, extra_headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    const LIMITED: &str = r#"{"error_code":5900,"error_message":"RATE_LIMIT_EXCEEDED"}"#;

    fn test_client(base: &str) -> Client {
        let http = reqwest::blocking::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        Client {
            uploader: http.clone(),
            http,
            base: base.to_string(),
            workspace: "ws".to_string(),
            key: "test-key".to_string(),
            sleep: record_sleep,
        }
    }

    #[test]
    fn send_retries_a_429_and_resends_the_write() {
        let (base, server) = serve(vec![
            response("429 Too Many Requests", "Retry-After: 3\r\n", LIMITED),
            response("429 Too Many Requests", "", LIMITED),
            response("201 Created", "", r#"{"id":"x"}"#),
        ]);
        let c = test_client(&base);
        let out = c.post("issues/", &json!({"name": "n"})).unwrap();
        assert_eq!(out, json!({"id": "x"}));
        // Retry-After first, then the backoff table's second step.
        assert_eq!(slept(), [3, 4]);
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 3);
        for (line, body) in &seen {
            assert!(line.starts_with("POST /workspaces/ws/issues/ "), "{line}");
            assert_eq!(body, r#"{"name":"n"}"#);
        }
    }

    #[test]
    fn send_gives_up_on_a_429_after_the_budget() {
        let responses = (0..=RATE_LIMIT_BACKOFF.len())
            .map(|_| response("429 Too Many Requests", "", LIMITED))
            .collect();
        let (base, server) = serve(responses);
        let err = test_client(&base).get("projects/", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("429 Too Many Requests"), "{msg}");
        assert!(msg.contains("RATE_LIMIT_EXCEEDED"), "{msg}");
        assert!(msg.contains("wait a minute and retry"), "{msg}");
        assert_eq!(slept(), [2, 4, 8, 16, 32]);
        assert_eq!(server.join().unwrap().len(), RATE_LIMIT_BACKOFF.len() + 1);
    }

    #[test]
    fn send_does_not_retry_other_errors() {
        let (base, server) = serve(vec![response(
            "500 Internal Server Error",
            "",
            r#"{"error":"boom"}"#,
        )]);
        let err = test_client(&base)
            .patch("issues/x/", &json!({}))
            .unwrap_err();
        assert!(err.to_string().contains("500"), "{err}");
        assert!(slept().is_empty());
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn issue_ref_uppercases_the_identifier() {
        assert_eq!(IssueRef::parse("res-12").unwrap().as_str(), "RES-12");
        assert_eq!(IssueRef::parse(" RES-12 ").unwrap().as_str(), "RES-12");
    }

    #[test]
    fn issue_ref_names_its_project_identifier() {
        assert_eq!(IssueRef::parse("res-12").unwrap().identifier(), "RES");
        assert_eq!(IssueRef::parse("TEACH2-3").unwrap().identifier(), "TEACH2");
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
    fn a_member_is_found_by_first_name_display_name_or_email() {
        let members = vec![
            json!({"id": "r", "first_name": "Robin", "last_name": "", "display_name": "sylvain.s.personal.assistant", "email": "sylvain.s.personal.assistant@gmail.com"}),
            json!({"id": "s", "first_name": "sylvain", "last_name": "hellin", "display_name": "plane.sincere", "email": "plane.sincere@alias.hellin.me"}),
        ];
        // First name is the ergonomic handle when display names are ugly.
        assert_eq!(find_member(&members, "Robin").unwrap()["id"], "r");
        assert_eq!(find_member(&members, "sylvain").unwrap()["id"], "s");
        // Display name and email resolve too, case-insensitively.
        assert_eq!(find_member(&members, "PLANE.SINCERE").unwrap()["id"], "s");
        assert_eq!(
            find_member(&members, "sylvain.s.personal.assistant@gmail.com").unwrap()["id"],
            "r"
        );
        // "First Last" resolves.
        assert_eq!(find_member(&members, "sylvain hellin").unwrap()["id"], "s");
    }

    #[test]
    fn an_unknown_or_non_member_assignee_fails_with_the_roster() {
        let members = vec![
            json!({"id": "s", "first_name": "sylvain", "last_name": "hellin", "display_name": "plane.sincere", "email": "x@y.z"}),
        ];
        let err = find_member(&members, "Robin").unwrap_err().to_string();
        assert!(err.contains("added to the project"), "{err}");
        assert!(err.contains("sylvain"), "{err}");
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
