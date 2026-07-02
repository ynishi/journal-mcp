//! Env-var resolution for the `journal-mcp` binary's `FileProjection`
//! startup mode.
//!
//! This logic stays in the binary crate (rather than `journal-mcp-rmcp`)
//! because deciding whether/where to attach a `FileProjection` from
//! `JOURNAL_FILE_ENABLE` / `JOURNAL_FILE_OUTPUT_PATH` is an environment
//! concern of this entry point; `journal-mcp-rmcp` only accepts an
//! already-resolved `Option<PathBuf>` via `RunConfig`.

use std::path::{Path, PathBuf};

/// Pure function: resolve the startup `FileProjection` output path from
/// env-style inputs.
///
/// Inputs are passed explicitly so this function is unit-testable without
/// touching the real process environment.  Caller is responsible for actually
/// reading the env vars before delegating to this function (or for supplying
/// stub values from a test).
///
/// Returns `(Some(path), None)` when ENABLE is set — the path comes from
/// `path_env` when supplied (relative resolved against `project_root`,
/// absolute used as-is), otherwise defaults to
/// `<project_root>/workspace/journal.md`.
///
/// Returns `(None, Some(warn_msg))` when `path_env.is_some()` but ENABLE is
/// unset (strict gate: PATH alone does not attach; caller should emit the
/// `warn_msg` via `tracing::warn!`).
///
/// Returns `(None, None)` when neither ENABLE nor PATH are set (the default
/// since v0.4.0 — no projection attached, EventLog SoT only).
pub(crate) fn resolve_file_projection_path(
    project_root: &Path,
    enable_set: bool,
    path_env: Option<&std::ffi::OsStr>,
) -> (Option<PathBuf>, Option<&'static str>) {
    if !enable_set {
        if path_env.is_some() {
            return (
                None,
                Some(
                    "JOURNAL_FILE_OUTPUT_PATH is set but JOURNAL_FILE_ENABLE is unset; \
                     FileProjection NOT attached. Set JOURNAL_FILE_ENABLE to enable file output.",
                ),
            );
        }
        return (None, None);
    }
    let resolved = match path_env {
        Some(s) => {
            let pb = PathBuf::from(s);
            if pb.is_absolute() {
                pb
            } else {
                project_root.join(pb)
            }
        }
        None => project_root.join("workspace").join("journal.md"),
    };
    (Some(resolved), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (1) ENABLE unset + PATH unset → no attach, no warning.
    #[test]
    fn test_resolve_file_projection_path_neither_set() {
        let root = PathBuf::from("/tmp/proj");
        let (resolved, warn) = resolve_file_projection_path(&root, false, None);
        assert_eq!(resolved, None);
        assert_eq!(warn, None);
    }

    /// (2) ENABLE set + PATH unset → default `<project_root>/workspace/journal.md`.
    #[test]
    fn test_resolve_file_projection_path_enable_only_uses_default() {
        let root = PathBuf::from("/tmp/proj");
        let (resolved, warn) = resolve_file_projection_path(&root, true, None);
        assert_eq!(
            resolved,
            Some(PathBuf::from("/tmp/proj/workspace/journal.md"))
        );
        assert_eq!(warn, None);
    }

    /// (3) ENABLE set + relative PATH → resolved against `project_root`.
    #[test]
    fn test_resolve_file_projection_path_relative_resolves_to_root() {
        let root = PathBuf::from("/tmp/proj");
        let path_env = std::ffi::OsString::from("custom/journal.md");
        let (resolved, warn) =
            resolve_file_projection_path(&root, true, Some(path_env.as_os_str()));
        assert_eq!(resolved, Some(PathBuf::from("/tmp/proj/custom/journal.md")));
        assert_eq!(warn, None);
    }

    /// (4) ENABLE set + absolute PATH → used as-is.
    #[test]
    fn test_resolve_file_projection_path_absolute_is_used_as_is() {
        let root = PathBuf::from("/tmp/proj");
        let path_env = std::ffi::OsString::from("/var/log/journal.md");
        let (resolved, warn) =
            resolve_file_projection_path(&root, true, Some(path_env.as_os_str()));
        assert_eq!(resolved, Some(PathBuf::from("/var/log/journal.md")));
        assert_eq!(warn, None);
    }

    /// (5) ENABLE unset + PATH set → no attach, strict warning surfaced.
    #[test]
    fn test_resolve_file_projection_path_path_set_but_enable_unset_warns() {
        let root = PathBuf::from("/tmp/proj");
        let path_env = std::ffi::OsString::from("custom/journal.md");
        let (resolved, warn) =
            resolve_file_projection_path(&root, false, Some(path_env.as_os_str()));
        assert_eq!(
            resolved, None,
            "PATH alone must not attach a projection (strict gate)"
        );
        let msg = warn.expect("warn message must be produced when PATH is set without ENABLE");
        assert!(
            msg.contains("JOURNAL_FILE_ENABLE"),
            "warn message should mention the env var; got: {msg}"
        );
    }
}
