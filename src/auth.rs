//! Personal access token resolution.
//!
//! The whole point of the binary owning this step is that no shell workflow
//! has to run `PAT=$(pass-cli ...)` or stage the secret in `/tmp`. The token
//! is read into memory, sent as `X-API-Key`, and never printed.
//!
//! Three sources, in this order: `PLANE_API_KEY`, the `api_key` line of the
//! config file, then Proton Pass through `pass-cli`. The middle one is for a
//! machine with no Proton Pass session, where the alternative is the same
//! token exported from a shell rc file; pass-cli stays the better option
//! wherever it is set up, since it keeps the token off the disk entirely.

use anyhow::{bail, Context, Result};
use std::process::Command;

use crate::config;

/// Resolve the Plane PAT: `PLANE_API_KEY`, then the config file, then
/// Proton Pass.
pub fn api_key() -> Result<String> {
    resolve(from_env(), config::stored_api_key(), from_pass_cli)
}

/// The precedence rule itself, with the three sources injected so it is
/// testable without a Proton Pass session or a mutated environment.
fn resolve(
    from_env: Option<String>,
    from_file: Option<String>,
    from_pass: impl FnOnce() -> Result<String>,
) -> Result<String> {
    if let Some(key) = from_env {
        return Ok(key);
    }
    if let Some(key) = from_file {
        return Ok(key);
    }
    from_pass()
}

/// The variable, read before the file is touched so a one-off token works
/// even against a config file that fails to parse.
fn from_env() -> Option<String> {
    let key = std::env::var("PLANE_API_KEY").ok()?.trim().to_string();
    // An exported-but-empty variable is how a shell says "unset" by accident.
    (!key.is_empty()).then_some(key)
}

fn from_pass_cli() -> Result<String> {
    // Where in Proton Pass the token lives is configurable, because the entry
    // a vault happens to use is per person: hardcoding one makes the binary
    // work for exactly one user and fail confusingly for everyone else.
    let (vault, item, field) = config::pass_coords()?;
    // `PROTON_PASS_KEY_PROVIDER=fs` is not optional: without it pass-cli fails
    // to find the on-disk key, concludes the local data is compromised, and
    // force-logs-out the session, which then needs a one-time PAT to restore.
    // Setting it here means the binary is safe to call from any environment,
    // not just a shell that happens to export it.
    let out = Command::new("pass-cli")
        .env("PROTON_PASS_KEY_PROVIDER", "fs")
        .args([
            "item",
            "view",
            "--vault-name",
            &vault,
            "--item-title",
            &item,
            "--field",
            &field,
        ])
        .output()
        .context(
            "Failed to run `pass-cli`. Set PLANE_API_KEY, run `plane config set api_key <token>`, or install pass-cli in PATH.",
        )?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "pass-cli could not read {item}/{field} from vault \"{vault}\" ({}).\n{}\nCheck the session with `pass-cli vault list`, point pass_vault / pass_item / pass_field at the right entry with `plane config set`, or store the token itself with `plane config set api_key <token>`.",
            out.status,
            err.trim()
        );
    }

    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if key.is_empty() {
        bail!("pass-cli returned an empty {field} for {item}. Set PLANE_API_KEY, run `plane config set api_key <token>`, or re-check the vault entry.");
    }
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(key: &'static str) -> impl FnOnce() -> Result<String> {
        move || Ok(key.to_string())
    }

    #[test]
    fn the_variable_beats_the_file_which_beats_pass_cli() {
        let both = resolve(
            Some("from-env".into()),
            Some("from-file".into()),
            pass("from-pass"),
        );
        assert_eq!(both.unwrap(), "from-env");

        let file_only = resolve(None, Some("from-file".into()), pass("from-pass"));
        assert_eq!(file_only.unwrap(), "from-file");

        let neither = resolve(None, None, pass("from-pass"));
        assert_eq!(neither.unwrap(), "from-pass");
    }

    #[test]
    fn pass_cli_is_not_run_when_a_token_is_already_known() {
        // Not just slower: on a machine with no session, running it turns a
        // perfectly good token into an error.
        for stored in [
            (Some("from-env".to_string()), None),
            (None, Some("from-file".to_string())),
        ] {
            let key = resolve(stored.0, stored.1, || {
                panic!("pass-cli was consulted anyway")
            });
            assert!(key.is_ok());
        }
    }
}
