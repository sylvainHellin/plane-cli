//! Command implementations.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::client::{self, Client, IssueRef};
use crate::config::{self, ConfigFile, Key, Source};
use crate::markdown;
use crate::mime;
use crate::note;
use crate::output::{self, emit, field, required_field};

// ---- configuration ----

pub fn config_set(key: &str, value: &str, json: bool) -> Result<()> {
    let key = Key::parse(key)?;
    let mut file = ConfigFile::load()?;
    file.set(key, value)?;
    file.save()?;
    let path = config::path()?;

    // A variable shadowing what was just written is the one way this command
    // can look like it did nothing, so it says so instead of staying silent.
    let shadowed = config::resolve(key, &file).source == Source::Env;
    // Echoing a token back would put it in the scrollback of the terminal it
    // was pasted into, and in the log of anything scripting this command.
    let shown = if key.is_secret() {
        config::MASK.to_string()
    } else {
        file.get(key).unwrap_or_default().to_string()
    };
    emit(
        &json!({
            "path": path.display().to_string(),
            "key": key.name(),
            "value": if key.is_secret() { Value::Null } else { json!(shown) },
            "secret": key.is_secret(),
            "shadowed_by_env": shadowed,
        }),
        json,
        || {
            let mut line = format!("{} = {shown}  ->  {}", key.name(), path.display());
            if key.is_secret() {
                line.push_str(
                    "\nnote: stored in plaintext, in a file only this account can read (0600). Where a Proton Pass session exists, pass-cli keeps the token off the disk instead.",
                );
            }
            if shadowed {
                line.push_str(&format!(
                    "\nnote: {} is set in this environment and overrides the file.",
                    key.env()
                ));
            }
            line
        },
    );
    Ok(())
}

pub fn config_unset(key: &str, json: bool) -> Result<()> {
    let key = Key::parse(key)?;
    let mut file = ConfigFile::load()?;
    let removed = file.unset(key);
    if removed {
        file.save()?;
    }
    let path = config::path()?;
    emit(
        &json!({
            "path": path.display().to_string(),
            "key": key.name(),
            "removed": removed,
        }),
        json,
        || {
            if removed {
                format!("{} removed from {}", key.name(), path.display())
            } else {
                format!("{} was not set in {}", key.name(), path.display())
            }
        },
    );
    Ok(())
}

pub fn config_show(json: bool) -> Result<()> {
    let file = ConfigFile::load()?;
    let path = config::path()?;
    let exists = path.exists();
    let rows = config::effective(&file);

    let path_text = path.display().to_string();
    emit(&config_show_json(&path_text, exists, &rows), json, || {
        output::config_show(&path_text, exists, &rows)
    });
    Ok(())
}

/// The `--json` view of the settings.
///
/// `rows` comes from `config::effective`, which has already replaced a stored
/// token with the mask, so neither view can print one. The JSON carries
/// `null` next to a `set` flag rather than the mask string: a consumer reads
/// whether there is a token, never a value it could send anywhere.
fn config_show_json(path: &str, exists: bool, rows: &[config::Resolved]) -> Value {
    let mut settings = Map::new();
    for r in rows {
        let entry = if r.key.is_secret() {
            json!({ "value": Value::Null, "set": r.value.is_some(), "source": r.source.label() })
        } else {
            json!({ "value": r.value, "source": r.source.label() })
        };
        settings.insert(r.key.name().to_string(), entry);
    }
    let api_key_set = rows
        .iter()
        .any(|r| r.key == Key::ApiKey && r.value.is_some());
    json!({
        "path": path,
        "exists": exists,
        "api_key_set": api_key_set,
        "settings": settings,
    })
}

pub fn config_path(json: bool) -> Result<()> {
    let path = config::path()?.display().to_string();
    emit(&json!({ "path": path }), json, || path.clone());
    Ok(())
}

// ---- reads ----

pub fn issue_get(reference: &str, json: bool) -> Result<()> {
    let c = Client::new()?;
    let r = IssueRef::parse(reference)?;
    // One call: the workspace-level identifier endpoint, state expanded in
    // place. No project lookup, no issue scan.
    let issue = c.issue_by_ref(&r)?;
    emit(&issue, json, || {
        output::issue_detail(&issue, r.as_str(), c.workspace())
    });
    Ok(())
}

pub fn issue_list(
    project: &str,
    state: Option<&str>,
    module: Option<&str>,
    label: Option<&str>,
    json: bool,
) -> Result<()> {
    let c = Client::new()?;
    let proj = c.project_by_identifier(project)?;
    let project_id = field(&proj, "id").to_string();
    let identifier = field(&proj, "identifier").to_string();

    let (mut issues, mut heading) = match module {
        Some(name) => {
            let module = c.find_module(&project_id, name)?;
            (
                c.module_issues(&project_id, field(&module, "id"))?,
                format!("{identifier} / {}", field(&module, "name")),
            )
        }
        None => (c.issues(&project_id)?, identifier.clone()),
    };

    if let Some(want) = state {
        // Resolve first, so a typo errors with the real list instead of
        // filtering everything out and reporting an empty project.
        let canonical = field(&c.find_state(&project_id, want)?, "name").to_string();
        let key = client::normalize(&canonical);
        issues.retain(|i| client::normalize(output::state_name(i)) == key);
        heading = format!("{heading} [{canonical}]");
    }

    if let Some(want) = label {
        // Same reasoning as --state: resolve first, so a typo names the real
        // labels instead of returning an empty list that reads as "none tagged".
        let canonical = field(&c.find_label(&project_id, want)?, "name").to_string();
        let key = client::normalize(&canonical);
        issues.retain(|i| {
            output::label_names(i)
                .iter()
                .any(|n| client::normalize(n) == key)
        });
        heading = format!("{heading} [{canonical}]");
    }

    issues.sort_by_key(|i| i.get("sequence_id").and_then(Value::as_i64).unwrap_or(0));

    emit(
        &json!({ "count": issues.len(), "results": issues }),
        json,
        || output::issue_list(&issues, &identifier, &heading),
    );
    Ok(())
}

pub fn project_list(json: bool) -> Result<()> {
    let c = Client::new()?;
    let mut projects = c.projects()?;
    projects.sort_by_key(|p| field(p, "identifier").to_string());
    emit(
        &json!({ "count": projects.len(), "results": projects }),
        json,
        || output::project_list(&projects),
    );
    Ok(())
}

pub fn module_list(project: &str, json: bool) -> Result<()> {
    let c = Client::new()?;
    let proj = c.project_by_identifier(project)?;
    let identifier = field(&proj, "identifier").to_string();
    let mut modules = c.modules(field(&proj, "id"))?;
    modules.sort_by_key(|m| field(m, "name").to_lowercase());
    emit(
        &json!({ "count": modules.len(), "results": modules }),
        json,
        || output::module_list(&modules, &identifier),
    );
    Ok(())
}

pub fn label_list(project: &str, json: bool) -> Result<()> {
    let c = Client::new()?;
    let proj = c.project_by_identifier(project)?;
    let identifier = field(&proj, "identifier").to_string();
    let mut labels = c.labels(field(&proj, "id"))?;
    labels.sort_by_key(|l| field(l, "name").to_lowercase());
    emit(
        &json!({ "count": labels.len(), "results": labels }),
        json,
        || output::label_list(&labels, &identifier),
    );
    Ok(())
}

pub fn state_list(project: &str, json: bool) -> Result<()> {
    let c = Client::new()?;
    let proj = c.project_by_identifier(project)?;
    let identifier = field(&proj, "identifier").to_string();
    let states = c.states(field(&proj, "id"))?;
    emit(
        &json!({ "count": states.len(), "results": states }),
        json,
        || output::state_list(&states, &identifier),
    );
    Ok(())
}

// ---- writes ----

pub struct CreateArgs {
    pub project: Option<String>,
    pub title: Option<String>,
    pub from_note: Option<PathBuf>,
    pub module: Option<String>,
    pub state: Option<String>,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub desc_md: Option<String>,
    pub labels: Vec<String>,
    pub json: bool,
}

pub fn issue_create(args: CreateArgs) -> Result<()> {
    // With --from-note the project positional is absent, so clap puts the
    // single remaining positional (the title) into `project`.
    let (title, project_arg) = match &args.from_note {
        Some(_) => {
            if args.title.is_some() {
                bail!("Pass either a project or --from-note, not both.");
            }
            (args.project.clone(), None)
        }
        None => (args.title.clone(), args.project.clone()),
    };
    let title = title.ok_or_else(|| anyhow!("A title is required."))?;

    let c = Client::new()?;

    // Resolve the target project and the module to attach to.
    let (project_id, mut identifier, mut module_id, mut module_label) = match &args.from_note {
        Some(path) => {
            let refs = note::read(path)?;
            // A note with no plane_module_id creates at project level. That is
            // the documented shape of five bridged notes, not an error.
            let label = refs.module_name.clone();
            (
                refs.project_id,
                refs.project_identifier,
                refs.module_id,
                label,
            )
        }
        None => {
            let ident = project_arg
                .ok_or_else(|| anyhow!("A project identifier is required (or use --from-note)."))?;
            let proj = c.project_by_identifier(&ident)?;
            (
                field(&proj, "id").to_string(),
                Some(field(&proj, "identifier").to_string()),
                None,
                None,
            )
        }
    };

    // An explicit --module always wins over the note's module.
    if let Some(name) = &args.module {
        module_id = Some(c.resolve_module(&project_id, name)?);
        module_label = Some(name.clone());
    }

    let label_ids = match args.labels.is_empty() {
        true => None,
        false => Some(resolve_label_ids(&c, &project_id, &args.labels)?),
    };
    let body = write_body(IssueFields {
        name: Some(title.clone()),
        description_html: match &args.desc_md {
            Some(md) => Some(markdown::to_html(&read_markdown(md)?)),
            None => None,
        },
        state_id: match &args.state {
            Some(s) => Some(c.resolve_state(&project_id, s)?),
            None => None,
        },
        priority: match &args.priority {
            Some(p) => Some(client::normalize_priority(p)?),
            None => None,
        },
        target_date: match &args.due {
            Some(d) => Some(client::validate_date(d)?),
            None => None,
        },
        label_ids: label_ids.clone(),
    });

    let created = c.create_issue(&project_id, &Value::Object(body))?;
    let issue_id = required_field(&created, "id", "the created issue")?.to_string();
    let seq = created
        .get("sequence_id")
        .and_then(Value::as_i64)
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());

    // A note may carry the project UUID without its identifier; look it up so
    // the printed reference is usable.
    if identifier.is_none() {
        identifier = c
            .projects()?
            .iter()
            .find(|p| field(p, "id") == project_id)
            .map(|p| field(p, "identifier").to_string());
    }
    let reference = format!("{}-{seq}", identifier.as_deref().unwrap_or("?"));

    // Unlike `update`, nothing here re-reads the issue afterwards, and
    // `labels` is the one field whose plausible misspelling answers 200 and
    // writes nothing. Count what came back rather than assume it landed. The
    // array holds ids on a create response and objects on an expanded read,
    // so only its length is read.
    let wanted_labels = label_ids.as_ref().map_or(0, Vec::len);
    let written_labels = created
        .get("labels")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let labels_short = written_labels < wanted_labels;

    // Attaching to a module is a second call: POST /issues/ has no module
    // field. Failure here leaves a real issue behind, so say so explicitly
    // rather than letting a retry create a duplicate.
    let mut attach_error = None;
    if let Some(mid) = &module_id {
        if let Err(e) = c.add_issue_to_module(&project_id, mid, &issue_id) {
            attach_error = Some(e);
        }
    }

    if args.json {
        emit(&created, true, String::new);
    } else {
        let mut out = format!("Created {reference}  {}", field(&created, "name"));
        if let Some(url) = output::issue_url(c.workspace(), &reference) {
            out.push_str(&format!("\n  url:      {url}"));
        }
        if wanted_labels > 0 && !labels_short {
            out.push_str(&format!("\n  labels:   {}", args.labels.join(", ")));
        }
        // A note may carry `plane_module_id` without `plane_module`, and an
        // attach that is not reported reads as one that did not happen.
        if let Some(label) = module_label.as_deref().or(module_id.as_deref()) {
            if attach_error.is_none() {
                out.push_str(&format!("\n  module:   {label} (attached)"));
            }
        }
        println!("{out}");
    }

    if let Some(e) = attach_error {
        bail!(
            "{reference} was created, but attaching it to module \"{}\" failed: {e}\nThe issue exists: link it with `plane issue update {reference} --module \"{}\"` rather than creating it again.",
            module_label.as_deref().unwrap_or("?"),
            module_label.as_deref().unwrap_or("?")
        );
    }
    if labels_short {
        bail!(
            "{reference} was created, but Plane returned {written_labels} of the {wanted_labels} labels it was given, so they did not all land.\nThe issue exists: set them with `plane issue update {reference} --label ...` rather than creating it again."
        );
    }
    Ok(())
}

/// The fields of an issue write, already resolved to what the API takes.
#[derive(Default)]
struct IssueFields {
    name: Option<String>,
    description_html: Option<String>,
    state_id: Option<String>,
    priority: Option<String>,
    target_date: Option<String>,
    label_ids: Option<Vec<String>>,
}

/// Assemble an issue write body. Pure, and separate from the request, so the
/// field *names* are pinned by a test rather than by a live call.
///
/// The one that matters is `labels`. Its plausible spelling `label_ids` is
/// accepted with a 200 and silently ignored, so renaming this key would stop
/// writing labels without failing anything.
fn write_body(f: IssueFields) -> Map<String, Value> {
    let mut body = Map::new();
    if let Some(v) = f.name {
        body.insert("name".into(), json!(v));
    }
    if let Some(v) = f.description_html {
        body.insert("description_html".into(), json!(v));
    }
    if let Some(v) = f.state_id {
        body.insert("state".into(), json!(v));
    }
    if let Some(v) = f.priority {
        body.insert("priority".into(), json!(v));
    }
    if let Some(v) = f.target_date {
        body.insert("target_date".into(), json!(v));
    }
    if let Some(v) = f.label_ids {
        body.insert("labels".into(), json!(v));
    }
    body
}

/// Resolve label names to the UUIDs the API writes, in one fetch.
///
/// Named for what it does rather than for `label_ids`, which is the request
/// key that looks plausible, is accepted with a 200, and is silently ignored:
/// verified against a live instance, where a `label_ids` PATCH left the issue
/// with no labels at all.
fn resolve_label_ids(c: &Client, project_id: &str, names: &[String]) -> Result<Vec<String>> {
    c.find_labels(project_id, names)?
        .iter()
        .map(|l| {
            l.get("id")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or_else(|| anyhow!("Label \"{}\" has no id", field(l, "name")))
        })
        .collect()
}

pub struct UpdateArgs {
    pub reference: String,
    pub state: Option<String>,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub title: Option<String>,
    pub module: Option<String>,
    pub labels: Vec<String>,
    pub json: bool,
}

pub fn issue_update(args: UpdateArgs) -> Result<()> {
    if args.state.is_none()
        && args.priority.is_none()
        && args.due.is_none()
        && args.title.is_none()
        && args.module.is_none()
        && args.labels.is_empty()
    {
        bail!("Nothing to update. Pass at least one of --state, --priority, --due, --title, --module, --label.");
    }

    let c = Client::new()?;
    let r = IssueRef::parse(&args.reference)?;
    let issue = c.issue_by_ref(&r)?;
    let issue_id = required_field(&issue, "id", r.as_str())?.to_string();
    let project_id = required_field(&issue, "project", r.as_str())?.to_string();

    // `labels` replaces the issue's whole set rather than adding to it, which
    // is verified live: patching one label onto an issue carrying two left it
    // with one. So every `--label` of a single invocation goes in the same
    // array, and a second invocation is a fresh set, not a second helping.
    let body = write_body(IssueFields {
        name: args.title.clone(),
        state_id: match &args.state {
            Some(s) => Some(c.resolve_state(&project_id, s)?),
            None => None,
        },
        priority: match &args.priority {
            Some(p) => Some(client::normalize_priority(p)?),
            None => None,
        },
        target_date: match &args.due {
            Some(d) => Some(client::validate_date(d)?),
            None => None,
        },
        label_ids: match args.labels.is_empty() {
            true => None,
            false => Some(resolve_label_ids(&c, &project_id, &args.labels)?),
        },
        ..IssueFields::default()
    });

    if !body.is_empty() {
        c.update_issue(&project_id, &issue_id, &Value::Object(body))
            .with_context(|| format!("Could not update {}", r.as_str()))?;
    }

    // `module` is not a field on the issue: the relation lives in a join
    // table, which is why the issue record keeps answering "module": null.
    if let Some(name) = &args.module {
        let module_id = c.resolve_module(&project_id, name)?;
        c.add_issue_to_module(&project_id, &module_id, &issue_id)
            .with_context(|| format!("Could not attach {} to module \"{name}\"", r.as_str()))?;
    }

    // Re-read so the output shows resolved names rather than the UUIDs we
    // just wrote.
    let updated = c.issue_by_ref(&r)?;
    emit(&updated, args.json, || {
        format!(
            "Updated {}\n{}",
            r.as_str(),
            output::issue_detail(&updated, r.as_str(), c.workspace())
        )
    });
    Ok(())
}

pub fn issue_comment(reference: &str, text: &str, json: bool) -> Result<()> {
    let c = Client::new()?;
    let r = IssueRef::parse(reference)?;
    let issue = c.issue_by_ref(&r)?;
    let body = read_markdown(text)?;
    let created = c.create_comment(
        required_field(&issue, "project", r.as_str())?,
        required_field(&issue, "id", r.as_str())?,
        &markdown::to_html(&body),
    )?;
    emit(&created, json, || format!("Commented on {}", r.as_str()));
    Ok(())
}

// ---- attachments ----

pub fn issue_attach(reference: &str, files: &[PathBuf], json: bool) -> Result<()> {
    let c = Client::new()?;
    let r = IssueRef::parse(reference)?;
    let issue = c.issue_by_ref(&r)?;
    let issue_id = required_field(&issue, "id", r.as_str())?.to_string();
    let project_id = required_field(&issue, "project", r.as_str())?.to_string();

    // Uploading several files is several independent three-call sequences,
    // not a transaction: file two failing does not undo file one. So each
    // success is reported the moment it lands, and a failure names what is
    // already attached, because a blind retry of the whole list attaches
    // those files a second time.
    let mut done = Vec::new();
    for path in files {
        match attach_one(&c, &project_id, &issue_id, path, r.as_str()) {
            Ok(entry) => {
                if !json {
                    println!("{}", attached_line(&entry, r.as_str()));
                }
                done.push(entry);
            }
            Err(e) => {
                if done.is_empty() {
                    return Err(e);
                }
                let names: Vec<&str> = done.iter().map(|a| field(a, "name")).collect();
                return Err(e.context(format!(
                    "{} of {} files already landed on {} ({}). `attach` is not transactional, so a retry should list only the files that are missing.",
                    names.len(),
                    files.len(),
                    r.as_str(),
                    names.join(", ")
                )));
            }
        }
    }

    // The human lines are already out; only the envelope is left to print.
    if json {
        emit(
            &json!({ "count": done.len(), "results": done }),
            true,
            String::new,
        );
    }
    Ok(())
}

/// Upload one file: request a slot, POST it, confirm.
fn attach_one(
    c: &Client,
    project_id: &str,
    issue_id: &str,
    path: &Path,
    reference: &str,
) -> Result<Value> {
    let size = std::fs::metadata(path)
        .with_context(|| format!("Could not read {}", path.display()))?
        .len();
    let (name, mime) = attachment_meta(path, size)?;

    // Three calls, and only the first two can leave a mess: an attachment
    // whose confirm never landed shows up as `is_uploaded: false` in
    // `plane issue attachments`, so a failure here is visible rather than
    // silent.
    let requested = c
        .request_attachment(project_id, issue_id, &name, size, mime)
        .with_context(|| format!("Could not request an upload slot for {name}"))?;
    let upload_data = requested
        .get("upload_data")
        .ok_or_else(|| anyhow!("Plane's response for {name} carries no `upload_data`."))?;
    let asset_id = required_field(&requested, "asset_id", &name)?.to_string();

    c.upload_attachment(upload_data, path, mime)?;
    c.confirm_attachment(project_id, issue_id, &asset_id)
        .with_context(|| {
            format!("{name} uploaded but the confirm failed, so it stays invisible on {reference}")
        })?;

    Ok(json!({
        "id": asset_id,
        "name": name,
        "size": size,
        "type": mime,
        "asset_url": requested.get("asset_url").cloned().unwrap_or(Value::Null),
    }))
}

fn attached_line(entry: &Value, reference: &str) -> String {
    format!(
        "Attached {} ({}, {}) to {reference}",
        field(entry, "name"),
        output::human_size(entry.get("size").and_then(Value::as_u64).unwrap_or(0) as f64),
        field(entry, "type")
    )
}

pub fn issue_attachments(reference: &str, json: bool) -> Result<()> {
    let c = Client::new()?;
    let r = IssueRef::parse(reference)?;
    let issue = c.issue_by_ref(&r)?;
    let mut attachments = c.attachments(
        required_field(&issue, "project", r.as_str())?,
        required_field(&issue, "id", r.as_str())?,
    )?;
    attachments.sort_by_key(|a| field(a, "created_at").to_string());
    emit(
        &json!({ "count": attachments.len(), "results": attachments }),
        json,
        || output::attachment_list(&attachments, r.as_str()),
    );
    Ok(())
}

/// The file name and MIME type to declare in step one of the upload.
///
/// Both are fixed into the presigned policy before a byte moves, and the
/// policy also pins the byte count with a minimum of 1, so an empty file is
/// rejected here with something legible rather than by S3 with a 403 and XML.
fn attachment_meta(path: &Path, size: u64) -> Result<(String, &'static str)> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
        .ok_or_else(|| {
            anyhow!(
                "{} has no usable file name to upload under.",
                path.display()
            )
        })?;
    if size == 0 {
        bail!(
            "{} is empty, and Plane's presigned upload requires at least one byte.",
            path.display()
        );
    }
    Ok((name.to_string(), mime::from_path(path)))
}

/// Take markdown from the argument, or from stdin when it is `-`.
fn read_markdown(arg: &str) -> Result<String> {
    if arg == "-" {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("Could not read markdown from stdin")?;
        Ok(buf)
    } else {
        Ok(arg.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_json_view_reports_that_a_token_is_set_without_carrying_it() {
        use crate::config::{Resolved, Source, MASK};

        let row = |key: Key, value: Option<&str>, source: Source| Resolved {
            key,
            value: value.map(str::to_string),
            source,
        };
        let rows = [
            row(Key::Workspace, Some("acme"), Source::File),
            // Masked by `config::effective` before it ever reaches here.
            row(Key::ApiKey, Some(MASK), Source::File),
        ];

        let out = config_show_json("/tmp/plane/config.toml", true, &rows);
        assert_eq!(out["api_key_set"], json!(true));
        assert_eq!(out["settings"]["api_key"]["value"], Value::Null);
        assert_eq!(out["settings"]["api_key"]["set"], json!(true));
        assert_eq!(out["settings"]["api_key"]["source"], json!("file"));
        // The mask is a thing to print, not a value to hand a JSON consumer.
        assert!(!out.to_string().contains(MASK), "{out}");
        // Ordinary settings still print their value.
        assert_eq!(out["settings"]["workspace"]["value"], json!("acme"));

        let rows = [row(Key::ApiKey, None, Source::Default)];
        let out = config_show_json("/tmp/plane/config.toml", true, &rows);
        assert_eq!(out["api_key_set"], json!(false));
        assert_eq!(out["settings"]["api_key"]["set"], json!(false));
        assert_eq!(out["settings"]["api_key"]["value"], Value::Null);
    }

    #[test]
    fn attachment_meta_takes_the_base_name_and_the_extension_mime() {
        let (name, mime) = attachment_meta(Path::new("/tmp/a b/Plan Ü.PDF"), 12).unwrap();
        assert_eq!(name, "Plan Ü.PDF");
        assert_eq!(mime, "application/pdf");
        let (_, mime) = attachment_meta(Path::new("Makefile"), 1).unwrap();
        assert_eq!(mime, mime::FALLBACK);
    }

    #[test]
    fn an_empty_file_is_refused_before_the_upload_slot_is_requested() {
        let err = attachment_meta(Path::new("/tmp/empty.txt"), 0)
            .unwrap_err()
            .to_string();
        assert!(err.contains("is empty"), "{err}");
        // A path that ends in `..` has no file name to upload under.
        assert!(attachment_meta(Path::new("/tmp/.."), 5).is_err());
    }

    #[test]
    fn labels_are_written_under_labels_never_under_label_ids() {
        let body = write_body(IssueFields {
            name: Some("T".into()),
            label_ids: Some(vec!["aaa".into(), "bbb".into()]),
            ..IssueFields::default()
        });
        // The whole point of the feature: `label_ids` is accepted with a 200
        // and silently writes nothing, so this key is pinned here rather than
        // discovered on a live instance months later.
        assert!(body.contains_key("labels"), "{body:?}");
        assert!(!body.contains_key("label_ids"), "{body:?}");
        assert_eq!(body["labels"], json!(["aaa", "bbb"]));
    }

    #[test]
    fn an_unset_field_is_absent_rather_than_null() {
        // A PATCH body is a delta: a key present with null would clear the
        // field instead of leaving it alone.
        let body = write_body(IssueFields {
            state_id: Some("s1".into()),
            ..IssueFields::default()
        });
        assert_eq!(body.keys().collect::<Vec<_>>(), vec!["state"]);
        assert!(write_body(IssueFields::default()).is_empty());
    }
}
