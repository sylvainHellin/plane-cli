//! Persistent configuration.
//!
//! Settings live in `$XDG_CONFIG_HOME/plane/config.toml`, falling back to
//! `~/.config/plane/config.toml`. A file rather than exported variables,
//! because every context that runs `plane` (an interactive shell, a systemd
//! user unit, an agent, a cron job) reads the same file, while an export only
//! reaches the processes that happen to descend from the shell that ran it.
//!
//! Precedence per setting: environment variable, then the file, then the
//! built-in default. So a one-off `PLANE_API_BASE=... plane project list`
//! still works without touching the file.
//!
//! `api_key` is the one setting that holds a credential, for machines with no
//! Proton Pass session where the alternative is an exported variable in a
//! shell rc file. It is a plaintext token in a `0600` file written through a
//! rename, so the file's own guarantees are what protects it, and nothing
//! that renders a setting is allowed to print it: `effective` hands every
//! renderer a redacted row (`Resolved::redacted`), which is why `config show`
//! can report that a token is set without being able to show it.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The API base of an instance reached over an SSH tunnel or on the same box.
pub const DEFAULT_API_BASE: &str = "http://localhost:8090/api/v1";
pub const DEFAULT_PASS_VAULT: &str = "Personal";
pub const DEFAULT_PASS_ITEM: &str = "plane";
pub const DEFAULT_PASS_FIELD: &str = "PAT";

/// A configurable setting. Closed on purpose: an unknown key is a typo, and
/// silently storing it would look like it took effect.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
// `ApiKey` ends with the enum's name, which clippy flags. Renaming it to
// `Token` would be the only variant not spelled like the key it stores.
#[allow(clippy::enum_variant_names)]
pub enum Key {
    Workspace,
    ApiBase,
    WebBase,
    PassVault,
    PassItem,
    PassField,
    ApiKey,
}

/// Every key, in the order `config show` prints them.
pub const KEYS: [Key; 7] = [
    Key::Workspace,
    Key::ApiBase,
    Key::WebBase,
    Key::PassVault,
    Key::PassItem,
    Key::PassField,
    Key::ApiKey,
];

/// What a credential's value renders as everywhere a value would be printed.
pub const MASK: &str = "(set)";

impl Key {
    pub fn name(self) -> &'static str {
        match self {
            Key::Workspace => "workspace",
            Key::ApiBase => "api_base",
            Key::WebBase => "web_base",
            Key::PassVault => "pass_vault",
            Key::PassItem => "pass_item",
            Key::PassField => "pass_field",
            Key::ApiKey => "api_key",
        }
    }

    /// Whether the value is a credential rather than a setting, so no output
    /// path may print it back.
    pub fn is_secret(self) -> bool {
        matches!(self, Key::ApiKey)
    }

    /// The variable that overrides this key for one invocation.
    pub fn env(self) -> &'static str {
        match self {
            Key::Workspace => "PLANE_WORKSPACE",
            Key::ApiBase => "PLANE_API_BASE",
            Key::WebBase => "PLANE_WEB_BASE",
            Key::PassVault => "PLANE_PASS_VAULT",
            Key::PassItem => "PLANE_PASS_ITEM",
            Key::PassField => "PLANE_PASS_FIELD",
            Key::ApiKey => "PLANE_API_KEY",
        }
    }

    /// `None` where guessing would be worse than failing: the workspace slug
    /// scopes every request path, and the web origin is not derivable from
    /// the API base when a proxy serves them on different hosts.
    pub fn default_value(self) -> Option<&'static str> {
        match self {
            // A token has no default either: there is nothing to guess, and
            // an unset one falls through to pass-cli rather than to a value.
            Key::Workspace | Key::WebBase | Key::ApiKey => None,
            Key::ApiBase => Some(DEFAULT_API_BASE),
            Key::PassVault => Some(DEFAULT_PASS_VAULT),
            Key::PassItem => Some(DEFAULT_PASS_ITEM),
            Key::PassField => Some(DEFAULT_PASS_FIELD),
        }
    }

    /// Parse a key as the user typed it. `PLANE_WEB_BASE`, `web-base` and
    /// `web_base` all name the same setting, since the README documents both
    /// spellings and retyping the one from the wrong column is expected.
    pub fn parse(input: &str) -> Result<Key> {
        let lowered = input
            .trim()
            .to_ascii_lowercase()
            .replace(['-', ' ', '.'], "_");
        let name = lowered.strip_prefix("plane_").unwrap_or(lowered.as_str());
        if let Some(key) = KEYS.iter().find(|k| k.name() == name) {
            return Ok(*key);
        }
        let shown = for_error(input);
        if looks_like_a_secret(name) {
            bail!(
                "`{shown}` is not a config key. The one credential this file holds is spelled `api_key`: `plane config set api_key <token>` stores it in plaintext for a machine with no pass-cli.\nOtherwise set PLANE_API_KEY for one-off runs, or point `pass_vault` / `pass_item` / `pass_field` at the Proton Pass entry holding the token."
            );
        }
        bail!("Unknown config key `{shown}`. Valid keys: {}.", key_list());
    }
}

fn key_list() -> String {
    KEYS.iter().map(|k| k.name()).collect::<Vec<_>>().join(", ")
}

/// How much of a rejected key an error may echo.
const MAX_ECHO: usize = 32;

/// A rejected key as the error prints it. Swapping the positionals
/// (`plane config set <token> workspace`) puts the PAT where the key belongs,
/// and the error outlives the command in a scrollback or a log, so the echo
/// is cut short: enough to recognize the typo, not enough to be the token.
fn for_error(input: &str) -> String {
    let trimmed = input.trim();
    let mut shown: String = trimmed.chars().take(MAX_ECHO).collect();
    if trimmed.chars().count() > MAX_ECHO {
        shown.push('…');
    }
    shown
}

/// Whether a rejected key name reads as a credential rather than as a typo,
/// so the error can point at the one spelling that is stored (`api_key`)
/// instead of listing the valid keys and leaving the user to try `pat` next.
fn looks_like_a_secret(name: &str) -> bool {
    const WORDS: [&str; 6] = ["key", "token", "secret", "password", "credential", "pat"];
    name.split('_').any(|part| WORDS.contains(&part))
        || ["apikey", "token", "secret", "password"]
            .iter()
            .any(|needle| name.contains(needle))
}

/// The file as it is on disk. Absent keys are absent, not empty strings, so
/// `unset` really removes a line rather than storing `""`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_vault: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_field: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
}

impl ConfigFile {
    fn slot(&mut self, key: Key) -> &mut Option<String> {
        match key {
            Key::Workspace => &mut self.workspace,
            Key::ApiBase => &mut self.api_base,
            Key::WebBase => &mut self.web_base,
            Key::PassVault => &mut self.pass_vault,
            Key::PassItem => &mut self.pass_item,
            Key::PassField => &mut self.pass_field,
            Key::ApiKey => &mut self.api_key,
        }
    }

    pub fn get(&self, key: Key) -> Option<&str> {
        let stored = match key {
            Key::Workspace => &self.workspace,
            Key::ApiBase => &self.api_base,
            Key::WebBase => &self.web_base,
            Key::PassVault => &self.pass_vault,
            Key::PassItem => &self.pass_item,
            Key::PassField => &self.pass_field,
            Key::ApiKey => &self.api_key,
        };
        stored.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }

    pub fn set(&mut self, key: Key, value: &str) -> Result<()> {
        let value = normalize(key, value.trim());
        if value.is_empty() {
            bail!(
                "`{}` cannot be set to an empty value. Use `plane config unset {}` to remove it.",
                key.name(),
                key.name()
            );
        }
        *self.slot(key) = Some(value);
        Ok(())
    }

    /// Remove a key, reporting whether it was there, so the caller can say
    /// "already unset" instead of implying it just deleted something.
    pub fn unset(&mut self, key: Key) -> bool {
        self.slot(key).take().is_some()
    }

    pub fn load_from(path: &Path) -> Result<ConfigFile> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ConfigFile::default()),
            Err(e) => return Err(e).with_context(|| format!("Failed to read {}", path.display())),
        };
        toml::from_str(&raw).with_context(|| {
            format!(
                "{} is not valid config; fix it by hand, or delete it and re-run `plane config set`",
                path.display()
            )
        })
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        let body = toml::to_string(self).context("Failed to serialize the config")?;
        let raw = format!(
            "# Written by `plane config set`. Kept private (0600): an `api_key` line, when there is one, is a token in plaintext.\n{body}"
        );

        // Written beside the target and renamed over it. A rename within one
        // directory is atomic, so an interrupted write leaves the previous
        // file whole instead of a truncated one that would then fail to parse
        // on the next run.
        let temp = temp_path(path);
        if let Err(e) = write_private(&temp, raw.as_bytes()) {
            let _ = std::fs::remove_file(&temp);
            return Err(e).with_context(|| format!("Failed to write {}", temp.display()));
        }
        if let Err(e) = std::fs::rename(&temp, path) {
            let _ = std::fs::remove_file(&temp);
            return Err(e).with_context(|| format!("Failed to write {}", path.display()));
        }
        Ok(())
    }

    pub fn load() -> Result<ConfigFile> {
        Self::load_from(&path()?)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&path()?)
    }
}

/// The scratch path `save_to` renames from, in the target's own directory so
/// the rename stays within one filesystem, and carrying the pid so two
/// processes writing at once cannot share it.
fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config.toml");
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

/// Create a file only this account can read.
///
/// Not a secret today, but it names the vault entry that holds one, and a
/// config file readable by every account on the box is a habit worth not
/// having. The mode rides the create call rather than a `set_permissions`
/// after the write, so the content is never briefly world-readable. The
/// explicit `set_permissions` still runs, because `mode` only applies to a
/// file this call creates and a scratch file left by a killed run would
/// otherwise keep whatever mode it already had.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Trailing slashes on the two origins, so `https://plane.example.com/` and
/// `https://plane.example.com` cannot produce URLs with a doubled separator.
fn normalize(key: Key, value: &str) -> String {
    match key {
        Key::ApiBase | Key::WebBase => value.trim_end_matches('/').to_string(),
        _ => value.to_string(),
    }
}

/// Where a value came from, which `config show` prints next to it: without
/// it, an env var shadowing the file looks like the file is wrong.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Env,
    File,
    Default,
}

impl Source {
    pub fn label(self) -> &'static str {
        match self {
            Source::Env => "env",
            Source::File => "file",
            Source::Default => "default",
        }
    }
}

/// One setting as it will actually be used. `value` is `None` only for the
/// two keys that have no default and were never set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub key: Key,
    pub value: Option<String>,
    pub source: Source,
}

impl Resolved {
    /// What `config show` prints in the source column. `default` would be a
    /// lie for the two keys that have none: nothing was applied, the setting
    /// is simply missing, and the difference is what the reader acts on.
    pub fn source_label(&self) -> &'static str {
        if self.value.is_none() {
            "unset"
        } else {
            self.source.label()
        }
    }

    /// The row with a credential's value replaced by [`MASK`].
    ///
    /// Applied once, in `effective`, rather than in each renderer: a row that
    /// never carries the token cannot leak it through a format string added
    /// later, and the source still says whether it came from the environment
    /// or from the file, which is the part worth reading.
    pub fn redacted(self) -> Resolved {
        if self.key.is_secret() && self.value.is_some() {
            return Resolved {
                value: Some(MASK.to_string()),
                ..self
            };
        }
        self
    }
}

pub fn resolve(key: Key, file: &ConfigFile) -> Resolved {
    resolve_with(key, file, |name| std::env::var(name).ok())
}

/// The precedence rule itself, with the environment injected so it is
/// testable without mutating the process the tests run in.
fn resolve_with(key: Key, file: &ConfigFile, env: impl Fn(&str) -> Option<String>) -> Resolved {
    let from_env = env(key.env())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    if let Some(value) = from_env {
        return Resolved {
            key,
            value: Some(normalize(key, &value)),
            source: Source::Env,
        };
    }
    if let Some(value) = file.get(key) {
        return Resolved {
            key,
            value: Some(normalize(key, value)),
            source: Source::File,
        };
    }
    Resolved {
        key,
        value: key.default_value().map(str::to_string),
        source: Source::Default,
    }
}

/// Every setting as `config show` sees it, credentials already redacted.
pub fn effective(file: &ConfigFile) -> Vec<Resolved> {
    KEYS.iter().map(|k| resolve(*k, file).redacted()).collect()
}

pub fn path() -> Result<PathBuf> {
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|home| home.join(".config"))
        })
        .ok_or_else(|| {
            anyhow!("Could not locate a config directory: neither XDG_CONFIG_HOME nor HOME is set.")
        })?;
    Ok(dir.join("plane").join("config.toml"))
}

/// The file, read once per process. Several call sites need it (the client
/// for the workspace and API base, auth for the vault coordinates, the
/// renderer for the web origin) and re-reading it per lookup would let one
/// invocation act on two different versions of the file.
fn cached() -> &'static Result<ConfigFile, String> {
    static CELL: OnceLock<Result<ConfigFile, String>> = OnceLock::new();
    CELL.get_or_init(|| ConfigFile::load().map_err(|e| format!("{e:#}")))
}

fn loaded() -> Result<&'static ConfigFile> {
    cached().as_ref().map_err(|e| anyhow!("{e}"))
}

/// The workspace slug, which every request path is scoped by.
pub fn workspace() -> Result<String> {
    // No default: guessing a slug turns a missing setting into a 404 against
    // someone else's workspace rather than into an answerable error.
    resolve(Key::Workspace, loaded()?).value.ok_or_else(|| {
        anyhow!("No workspace set. Run `plane config set workspace <slug>`, or set PLANE_WORKSPACE for one call. The slug is the path segment in the web UI URL, e.g. `acme` in https://plane.example.com/acme/browse/.")
    })
}

pub fn api_base() -> Result<String> {
    Ok(resolve(Key::ApiBase, loaded()?)
        .value
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string()))
}

/// The browser origin of the instance.
///
/// Unset means no `url:` line at all, which is honest: a placeholder would
/// print a link that silently goes nowhere.
pub fn web_base() -> Option<String> {
    let file = loaded().ok()?;
    resolve(Key::WebBase, file).value
}

/// The token stored in the file, if there is one.
///
/// The file only, not the resolved value: `auth` reads `PLANE_API_KEY`
/// itself, before anything touches the file, so a token in the environment
/// keeps working on a machine whose config file is malformed.
pub fn stored_api_key() -> Option<String> {
    loaded().ok()?.get(Key::ApiKey).map(str::to_string)
}

/// Where in Proton Pass the token lives, as vault, item, field.
pub fn pass_coords() -> Result<(String, String, String)> {
    let file = loaded()?;
    let value = |key: Key| {
        resolve(key, file)
            .value
            .unwrap_or_else(|| key.default_value().unwrap_or_default().to_string())
    };
    Ok((
        value(Key::PassVault),
        value(Key::PassItem),
        value(Key::PassField),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn env_beats_the_file_which_beats_the_default() {
        let mut file = ConfigFile::default();
        file.set(Key::ApiBase, "https://plane.example.com/api/v1")
            .unwrap();
        file.set(Key::PassItem, "plane-work").unwrap();

        // File over default.
        let r = resolve_with(Key::ApiBase, &file, no_env);
        assert_eq!(r.value.as_deref(), Some("https://plane.example.com/api/v1"));
        assert_eq!(r.source, Source::File);

        // Env over file.
        let env = env_of(&[("PLANE_API_BASE", "http://localhost:9000/api/v1")]);
        let r = resolve_with(Key::ApiBase, &file, &env);
        assert_eq!(r.value.as_deref(), Some("http://localhost:9000/api/v1"));
        assert_eq!(r.source, Source::Env);

        // Default when neither names it.
        let r = resolve_with(Key::PassVault, &file, no_env);
        assert_eq!(r.value.as_deref(), Some(DEFAULT_PASS_VAULT));
        assert_eq!(r.source, Source::Default);

        // Set in the file, untouched by an unrelated variable.
        let r = resolve_with(Key::PassItem, &file, &env);
        assert_eq!(r.value.as_deref(), Some("plane-work"));
        assert_eq!(r.source, Source::File);
    }

    #[test]
    fn the_two_keys_without_a_default_resolve_to_nothing() {
        let file = ConfigFile::default();
        for key in [Key::Workspace, Key::WebBase] {
            let r = resolve_with(key, &file, no_env);
            assert_eq!(r.value, None, "{}", key.name());
            assert_eq!(r.source, Source::Default);
        }
    }

    #[test]
    fn an_empty_variable_does_not_shadow_the_file() {
        // An exported-but-empty variable is how a shell says "unset" by
        // accident; treating it as a value would blank the workspace.
        let mut file = ConfigFile::default();
        file.set(Key::Workspace, "acme").unwrap();
        let env = env_of(&[("PLANE_WORKSPACE", "   ")]);
        let r = resolve_with(Key::Workspace, &file, &env);
        assert_eq!(r.value.as_deref(), Some("acme"));
        assert_eq!(r.source, Source::File);
    }

    #[test]
    fn origins_lose_their_trailing_slash_from_either_source() {
        let mut file = ConfigFile::default();
        file.set(Key::WebBase, "https://plane.example.com/")
            .unwrap();
        assert_eq!(file.get(Key::WebBase), Some("https://plane.example.com"));
        let env = env_of(&[("PLANE_API_BASE", "https://plane.example.com/api/v1/")]);
        let r = resolve_with(Key::ApiBase, &file, &env);
        assert_eq!(r.value.as_deref(), Some("https://plane.example.com/api/v1"));
    }

    #[test]
    fn another_spelling_of_the_token_is_refused_and_pointed_at_api_key() {
        // `api_key` is a real key now, so these are near misses rather than
        // attempts at something the file refuses to hold: the error has to
        // name the spelling that works instead of listing six unrelated keys.
        for attempt in [
            "apikey",
            "pat",
            "token",
            "auth-token",
            "secret",
            "password",
            "credential",
        ] {
            let err = Key::parse(attempt).unwrap_err().to_string();
            assert!(err.contains("`api_key`"), "{attempt}: {err}");
            assert!(err.contains("PLANE_API_KEY"), "{attempt}: {err}");
        }
    }

    #[test]
    fn the_token_key_parses_in_every_spelling_and_is_marked_secret() {
        for spelling in [
            "api_key",
            "api-key",
            "api.key",
            " API_KEY ",
            "PLANE_API_KEY",
        ] {
            assert_eq!(Key::parse(spelling).unwrap(), Key::ApiKey, "{spelling}");
        }
        assert!(Key::ApiKey.is_secret());
        assert_eq!(Key::ApiKey.env(), "PLANE_API_KEY");
        // No default: an unset token falls through to pass-cli, not to a value.
        assert_eq!(Key::ApiKey.default_value(), None);
        for key in KEYS.iter().filter(|k| **k != Key::ApiKey) {
            assert!(!key.is_secret(), "{}", key.name());
        }
    }

    #[test]
    fn the_token_round_trips_through_the_file_but_never_through_a_rendered_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut file = ConfigFile::default();
        file.set(Key::ApiKey, "plane_pat_abcdef123456").unwrap();
        file.save_to(&path).unwrap();
        let reread = ConfigFile::load_from(&path).unwrap();
        assert_eq!(reread.get(Key::ApiKey), Some("plane_pat_abcdef123456"));

        // Every renderer reads `effective`, and what it hands back is masked.
        let row = |rows: &[Resolved]| rows.iter().find(|r| r.key == Key::ApiKey).unwrap().clone();
        let shown = row(&effective(&reread));
        assert_eq!(shown.value.as_deref(), Some(MASK));
        assert_eq!(shown.source_label(), "file");

        // The environment shadows it, and the row says so without a value.
        let from_env = resolve_with(
            Key::ApiKey,
            &reread,
            env_of(&[("PLANE_API_KEY", "plane_pat_from_the_environment")]),
        )
        .redacted();
        assert_eq!(from_env.value.as_deref(), Some(MASK));
        assert_eq!(from_env.source_label(), "env");

        // Unset is unset, not a mask over nothing.
        let mut file = reread;
        assert!(file.unset(Key::ApiKey));
        file.save_to(&path).unwrap();
        let reread = ConfigFile::load_from(&path).unwrap();
        assert_eq!(reread.get(Key::ApiKey), None);
        let shown = row(&effective(&reread));
        assert_eq!(shown.value, None);
        assert_eq!(shown.source_label(), "unset");
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("plane_pat_"), "{raw}");
    }

    #[test]
    fn an_unknown_key_lists_the_valid_ones() {
        let err = Key::parse("workspce").unwrap_err().to_string();
        assert!(err.contains("Unknown config key `workspce`"), "{err}");
        for key in KEYS {
            assert!(err.contains(key.name()), "{err}");
        }
    }

    #[test]
    fn a_rejected_key_is_echoed_only_far_enough_to_recognize() {
        // What a swapped `plane config set <token> workspace` would echo.
        let long = "z".repeat(80);
        let err = Key::parse(&long).unwrap_err().to_string();
        assert!(
            err.contains(&format!("`{}…`", "z".repeat(MAX_ECHO))),
            "{err}"
        );
        assert!(!err.contains(&long), "{err}");

        // Short enough to be a typo: printed whole, without an ellipsis.
        let err = Key::parse("workspce").unwrap_err().to_string();
        assert!(err.contains("`workspce`"), "{err}");
    }

    #[test]
    fn keys_parse_in_every_spelling_the_docs_use() {
        assert_eq!(Key::parse("web_base").unwrap(), Key::WebBase);
        assert_eq!(Key::parse("web-base").unwrap(), Key::WebBase);
        assert_eq!(Key::parse(" WEB_BASE ").unwrap(), Key::WebBase);
        assert_eq!(Key::parse("PLANE_WEB_BASE").unwrap(), Key::WebBase);
    }

    #[test]
    fn set_and_unset_round_trip_through_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        // A file that is not there yet reads as empty rather than as an error.
        assert_eq!(ConfigFile::load_from(&path).unwrap(), ConfigFile::default());

        let mut file = ConfigFile::default();
        file.set(Key::Workspace, "acme").unwrap();
        file.set(Key::WebBase, "https://plane.example.com").unwrap();
        file.save_to(&path).unwrap();

        let reread = ConfigFile::load_from(&path).unwrap();
        assert_eq!(reread, file);
        assert_eq!(reread.get(Key::Workspace), Some("acme"));

        // Nothing writes a key that was not set, least of all the one that
        // would hold a token.
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("api_key = "), "{raw}");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        }

        let mut file = reread;
        assert!(file.unset(Key::WebBase));
        assert!(!file.unset(Key::WebBase), "removing twice removes nothing");
        file.save_to(&path).unwrap();
        let reread = ConfigFile::load_from(&path).unwrap();
        assert_eq!(reread.get(Key::WebBase), None);
        assert_eq!(reread.get(Key::Workspace), Some("acme"));

        // And the effective view labels each source correctly.
        let env = env_of(&[("PLANE_WEB_BASE", "https://other.example.com")]);
        let rows: Vec<Resolved> = KEYS
            .iter()
            .map(|k| resolve_with(*k, &reread, &env))
            .collect();
        let by = |key: Key| rows.iter().find(|r| r.key == key).unwrap().clone();
        assert_eq!(by(Key::Workspace).source, Source::File);
        assert_eq!(by(Key::WebBase).source, Source::Env);
        assert_eq!(by(Key::ApiBase).source, Source::Default);
    }

    #[cfg(unix)]
    #[test]
    fn the_file_is_created_private_however_permissive_the_umask_is() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let mut file = ConfigFile::default();
        file.set(Key::Workspace, "acme").unwrap();

        // Under umask 000 a plainly created file lands 0666, so this fails
        // unless the mode comes from the create call itself.
        let previous = unsafe { libc::umask(0o000) };
        let fresh = file.save_to(&path);
        // A file that was already there is replaced by the renamed scratch
        // file, so a 0644 predecessor cannot carry its mode over either.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let overwrite = file.save_to(&path);
        unsafe { libc::umask(previous) };
        fresh.unwrap();
        overwrite.unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");

        // And the scratch file is renamed, not left beside the real one.
        let mut entries: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        entries.sort();
        assert_eq!(entries, ["config.toml"]);
    }

    #[test]
    fn a_malformed_file_names_itself_instead_of_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        std::fs::write(&path, "workspace = \n").unwrap();
        let err = ConfigFile::load_from(&path).unwrap_err().to_string();
        assert!(err.contains("is not valid config"), "{err}");

        // A key that is not a setting is a typo, not something to ignore.
        std::fs::write(&path, "workspac = \"acme\"\n").unwrap();
        assert!(ConfigFile::load_from(&path).is_err());
    }

    #[test]
    fn an_empty_value_is_refused_and_points_at_unset() {
        let mut file = ConfigFile::default();
        let err = file.set(Key::Workspace, "   ").unwrap_err().to_string();
        assert!(err.contains("plane config unset workspace"), "{err}");
        assert_eq!(file.get(Key::Workspace), None);
    }
}
