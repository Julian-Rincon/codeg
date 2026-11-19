//! Where Phantom automations put their `WorktreePerRun` worktrees.
//!
//! Upstream drops them next to the project folder. For a project that lives
//! inside another git repo (e.g. `nexus/espacio`) that puts the worktrees inside
//! the outer repo, so we let them live in one shared directory instead.
//!
//! Cleanup does not depend on this location: a run's worktree is removed through
//! `git worktree list`, which reports the real path wherever it was created.

use std::path::{Path, PathBuf};

const ENV_DIR: &str = "PHANTOM_WORKTREES_DIR";
const DEFAULT_SUBDIR: &str = "Documentos/Proyectos/_worktrees";

/// Pure decision: `env_dir` wins when non-empty, otherwise `<home>/Documentos/Proyectos/_worktrees`
/// if that directory exists. `None` means "use the sibling path".
pub fn resolve(name: &str, env_dir: Option<&str>, home: Option<&Path>) -> Option<String> {
    let base: PathBuf = match env_dir.map(str::trim).filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => {
            let default = home?.join(DEFAULT_SUBDIR);
            if !default.is_dir() {
                return None;
            }
            default
        }
    };
    Some(base.join(name).to_string_lossy().into_owned())
}

/// Reads the real environment and home directory. Hook point for the engine.
pub fn path_for(name: &str) -> Option<String> {
    resolve(
        name,
        std::env::var(ENV_DIR).ok().as_deref(),
        dirs::home_dir().as_deref(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_dir_wins_even_if_default_exists() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(DEFAULT_SUBDIR)).unwrap();
        let got = resolve(
            "repo-automation-1-run-2",
            Some("/srv/wt"),
            Some(home.path()),
        );
        assert_eq!(got, Some("/srv/wt/repo-automation-1-run-2".to_string()));
    }

    #[test]
    fn default_dir_used_when_it_exists() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(DEFAULT_SUBDIR);
        std::fs::create_dir_all(&base).unwrap();
        let got = resolve("r-run-1", None, Some(home.path())).unwrap();
        assert_eq!(PathBuf::from(got), base.join("r-run-1"));
    }

    #[test]
    fn falls_back_when_default_missing_or_no_home() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(resolve("x", None, Some(home.path())), None);
        assert_eq!(resolve("x", None, None), None);
    }

    #[test]
    fn blank_env_is_ignored() {
        let home = tempfile::tempdir().unwrap();
        assert_eq!(resolve("x", Some("  "), Some(home.path())), None);
    }
}
