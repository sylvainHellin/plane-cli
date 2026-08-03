//! Personal access token resolution.
//!
//! The whole point of the binary owning this step is that no shell workflow
//! has to run `PAT=$(pass-cli ...)` or stage the secret in `/tmp`. The token
//! is read into memory, sent as `X-API-Key`, and never printed or persisted.

use anyhow::{bail, Context, Result};
use std::process::Command;

const DEFAULT_VAULT: &str = "Personal";
const DEFAULT_ITEM: &str = "plane";
const DEFAULT_FIELD: &str = "PAT";

/// Where in Proton Pass the token lives. Configurable, because the entry a
/// vault happens to use is per person: hardcoding one makes the binary work
/// for exactly one user and fail confusingly for everyone else.
fn coords() -> (String, String, String) {
    let var = |name: &str, default: &str| {
        std::env::var(name)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    (
        var("PLANE_PASS_VAULT", DEFAULT_VAULT),
        var("PLANE_PASS_ITEM", DEFAULT_ITEM),
        var("PLANE_PASS_FIELD", DEFAULT_FIELD),
    )
}

/// Resolve the Plane PAT: `PLANE_API_KEY` wins, then Proton Pass.
pub fn api_key() -> Result<String> {
    if let Ok(key) = std::env::var("PLANE_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }
    from_pass_cli()
}

fn from_pass_cli() -> Result<String> {
    let (vault, item, field) = coords();
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
            "Failed to run `pass-cli`. Set PLANE_API_KEY instead, or install pass-cli in PATH.",
        )?;

    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        bail!(
            "pass-cli could not read {item}/{field} from vault \"{vault}\" ({}).\n{}\nCheck the session with `pass-cli vault list`, point PLANE_PASS_VAULT / PLANE_PASS_ITEM / PLANE_PASS_FIELD at the right entry, or set PLANE_API_KEY.",
            out.status,
            err.trim()
        );
    }

    let key = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if key.is_empty() {
        bail!("pass-cli returned an empty {field} for {item}. Set PLANE_API_KEY, or re-check the vault entry.");
    }
    Ok(key)
}
