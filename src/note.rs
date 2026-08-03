//! Reading Plane IDs out of an Obsidian note's frontmatter.
//!
//! The vault-to-Plane mapping is asymmetric (a vault *project* is a Plane
//! *module*, a vault *area* is a Plane *project*), but nothing here has to
//! know that: every bridged note denormalizes both UUIDs into its own YAML
//! header, so this is two string reads and no traversal.
//!
//! `plane_module_id` is optional on purpose. A bridged note need not belong
//! to a module, and those must create at project level rather than fail.

use anyhow::{bail, Context, Result};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct NoteRefs {
    pub project_id: String,
    pub module_id: Option<String>,
    /// `plane_project` (e.g. `RES`), for output only.
    pub project_identifier: Option<String>,
    /// `plane_module` (e.g. `Paper 2 Drawings`), for output only.
    pub module_name: Option<String>,
}

pub fn read(path: &Path) -> Result<NoteRefs> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read note {}", path.display()))?;
    let front = frontmatter(&raw).ok_or_else(|| {
        anyhow::anyhow!(
            "{} has no YAML frontmatter, so it carries no Plane IDs.",
            path.display()
        )
    })?;

    let project_id = scalar(front, "plane_project_id");
    let Some(project_id) = project_id else {
        bail!(
            "{} has no `plane_project_id` in its frontmatter, so there is no Plane project to create in.",
            path.display()
        );
    };
    let project_id = check_uuid(&project_id, "plane_project_id", path)?;
    let module_id = match scalar(front, "plane_module_id") {
        Some(id) => Some(check_uuid(&id, "plane_module_id", path)?),
        None => None,
    };

    Ok(NoteRefs {
        project_id,
        module_id,
        project_identifier: scalar(front, "plane_project"),
        module_name: scalar(front, "plane_module"),
    })
}

/// Both IDs are spliced straight into a request path, so a note that carries
/// `../../../workspaces/other/projects/deadbeef` would steer the write out of
/// this workspace, and `abc?x=1` would inject a query string. Vault notes are
/// semi-trusted, and the shape is fixed, so require the canonical UUID.
fn check_uuid(value: &str, key: &str, path: &Path) -> Result<String> {
    if !is_uuid(value) {
        bail!(
            "{} has `{key}: {value}`, which is not a UUID. Plane IDs are 36 characters, e.g. 48284b59-0000-4000-8000-000000000000.",
            path.display()
        );
    }
    Ok(value.to_string())
}

/// The canonical 8-4-4-4-12 hex form, case-insensitive.
fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && [8, 4, 4, 4, 12]
            .iter()
            .zip(&groups)
            .all(|(len, g)| g.len() == *len && g.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Return the frontmatter block of a note, without its `---` fences.
fn frontmatter(raw: &str) -> Option<&str> {
    let rest = raw
        .strip_prefix("---\n")
        .or_else(|| raw.strip_prefix("---\r\n"))?;
    let end = rest.find("\n---")?;
    Some(&rest[..end])
}

/// Read a top-level scalar key. Quotes are stripped; empty values read as
/// absent, which is how the vault spells "no module".
fn scalar(front: &str, key: &str) -> Option<String> {
    for line in front.lines() {
        // Top-level keys only: an indented line belongs to a nested block.
        if line.starts_with([' ', '\t', '-']) {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim().trim_matches(['"', '\'']).trim();
        return if v.is_empty() {
            None
        } else {
            Some(v.to_string())
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOTE: &str = "---\ntype: project\naliases:\n  - Widgets\nplane_workspace: acme\nplane_project: ACME\nplane_project_id: 3f00e638-0000-4000-8000-000000000000\nplane_module: Widget Redesign\nplane_module_id: 17f8619e-0000-4000-8000-000000000000\n---\n# body\n";

    #[test]
    fn reads_both_ids() {
        let front = frontmatter(NOTE).unwrap();
        assert_eq!(
            scalar(front, "plane_project_id").as_deref(),
            Some("3f00e638-0000-4000-8000-000000000000")
        );
        assert_eq!(
            scalar(front, "plane_module_id").as_deref(),
            Some("17f8619e-0000-4000-8000-000000000000")
        );
        assert_eq!(
            scalar(front, "plane_module").as_deref(),
            Some("Widget Redesign")
        );
    }

    #[test]
    fn a_missing_module_reads_as_none_not_an_error() {
        // A bridged note with no module at all, which is a real shape.
        let note = "---\nplane_project: DOCS\nplane_project_id: 77e32b15-0000-4000-8000-000000000000\n---\nbody\n";
        let front = frontmatter(note).unwrap();
        assert_eq!(scalar(front, "plane_module_id"), None);
    }

    #[test]
    fn an_empty_value_reads_as_absent() {
        let front = frontmatter("---\nend_by:\nplane_module_id:\n---\n").unwrap();
        assert_eq!(scalar(front, "plane_module_id"), None);
    }

    #[test]
    fn quoted_values_are_unwrapped_and_nested_keys_ignored() {
        let front = frontmatter("---\narea: \"[[teaching]]\"\naliases:\n  - plane_module_id: nope\nplane_module_id: real\n---\n").unwrap();
        assert_eq!(scalar(front, "area").as_deref(), Some("[[teaching]]"));
        assert_eq!(scalar(front, "plane_module_id").as_deref(), Some("real"));
    }

    #[test]
    fn ids_that_are_not_uuids_are_refused() {
        assert!(is_uuid("3f00e638-0000-4000-8000-0000000000ff"));
        assert!(is_uuid("3F00E638-0000-4000-8000-0000000000FF"));
        for bad in [
            // Path traversal out of the workspace, seen in review.
            "../../../workspaces/other/projects/deadbeef",
            // Query-string injection into the URL.
            "abc?x=1",
            "3f00e638-0000-4000-8000-0000000000ff/../x",
            "3f00e638-0000-4000-8000-0000000000f",
            "3f00e638_0000_4000_8000_0000000000ff",
            "gggggggg-0000-4000-8000-0000000000ff",
            "",
        ] {
            assert!(!is_uuid(bad), "should reject {bad}");
        }
    }

    #[test]
    fn a_traversing_id_fails_the_read_with_the_note_named() {
        let dir = std::env::temp_dir().join(format!("plane-cli-note-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let note = dir.join("bad.md");
        std::fs::write(
            &note,
            "---\nplane_project_id: ../../../workspaces/other/projects/deadbeef\n---\nbody\n",
        )
        .unwrap();
        let err = read(&note).unwrap_err().to_string();
        assert!(err.contains("is not a UUID"), "{err}");
        std::fs::remove_file(&note).unwrap();
    }

    #[test]
    fn a_note_without_frontmatter_is_detected() {
        assert!(frontmatter("# just a heading\n").is_none());
    }
}
