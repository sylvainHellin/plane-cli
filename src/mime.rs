//! Extension-to-MIME mapping for attachment uploads.
//!
//! Plane's presigned upload wants a `type` in step one and echoes it into the
//! S3 policy, so the value has to be decided before the file moves. A table
//! of the extensions this board actually sees is enough: it costs no
//! dependency, and anything unlisted falls back to `application/octet-stream`,
//! which uploads and downloads correctly, it just does not preview in the
//! browser.

use std::path::Path;

pub const FALLBACK: &str = "application/octet-stream";

/// Extensions worth naming, lowercase and without the dot.
const TABLE: &[(&str, &str)] = &[
    ("pdf", "application/pdf"),
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("heic", "image/heic"),
    ("txt", "text/plain"),
    ("md", "text/markdown"),
    ("csv", "text/csv"),
    ("html", "text/html"),
    ("css", "text/css"),
    ("json", "application/json"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
    ("xml", "application/xml"),
    ("zip", "application/zip"),
    ("gz", "application/gzip"),
    ("tar", "application/x-tar"),
    ("doc", "application/msword"),
    (
        "docx",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    ),
    ("xls", "application/vnd.ms-excel"),
    (
        "xlsx",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    ),
    ("ppt", "application/vnd.ms-powerpoint"),
    (
        "pptx",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
    ),
    ("ifc", "application/x-step"),
    ("dwg", "image/vnd.dwg"),
    ("dxf", "image/vnd.dxf"),
    ("mp3", "audio/mpeg"),
    ("m4a", "audio/mp4"),
    ("wav", "audio/wav"),
    ("mp4", "video/mp4"),
    ("mov", "video/quicktime"),
];

/// The MIME type for a path, by extension, case-insensitively.
pub fn from_path(path: &Path) -> &'static str {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return FALLBACK;
    };
    let ext = ext.to_lowercase();
    TABLE
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mime)| *mime)
        .unwrap_or(FALLBACK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn known_extensions_map_case_insensitively() {
        assert_eq!(
            from_path(&PathBuf::from("a/b/report.pdf")),
            "application/pdf"
        );
        assert_eq!(from_path(&PathBuf::from("SCAN.PNG")), "image/png");
        assert_eq!(from_path(&PathBuf::from("notes.MD")), "text/markdown");
        assert_eq!(from_path(&PathBuf::from("model.ifc")), "application/x-step");
    }

    #[test]
    fn anything_else_falls_back_rather_than_failing() {
        // No extension, an unknown one, and a dotfile that is all extension.
        assert_eq!(from_path(&PathBuf::from("Makefile")), FALLBACK);
        assert_eq!(from_path(&PathBuf::from("model.rvt")), FALLBACK);
        assert_eq!(from_path(&PathBuf::from(".gitignore")), FALLBACK);
    }

    #[test]
    fn the_table_has_no_duplicate_extensions() {
        // Two rows for one extension would make the mapping order-dependent.
        let mut seen: Vec<&str> = TABLE.iter().map(|(e, _)| *e).collect();
        seen.sort_unstable();
        let before = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), before);
    }
}
