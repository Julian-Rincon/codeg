//! NEXUS command center: the data behind Phantom's home screen.
//!
//! `nexus panel` (a timer in the NEXUS repo) writes `~/.local/share/nexus/panel.json`;
//! this command only hands that file to the webview. Phantom knows nothing about
//! calendars, tasks or memory — if the file is missing, unreadable or too big the
//! home screen simply shows its usual greeting.

use std::path::{Path, PathBuf};

use crate::app_error::AppCommandError;

/// A panel is a few kilobytes; anything near this is not a panel.
const MAX_PANEL_BYTES: u64 = 256 * 1024;

fn panel_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".local/share/nexus/panel.json"))
}

fn read_panel(path: &Path) -> Option<serde_json::Value> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > MAX_PANEL_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    value.is_object().then_some(value)
}

/// The panel, or `None` when NEXUS has not written one.
pub async fn nexus_panel_core() -> Result<Option<serde_json::Value>, AppCommandError> {
    let Some(path) = panel_path() else {
        return Ok(None);
    };
    Ok(tokio::task::spawn_blocking(move || read_panel(&path))
        .await
        .unwrap_or(None))
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn nexus_panel() -> Result<Option<serde_json::Value>, AppCommandError> {
    nexus_panel_core().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_object_and_refuses_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("panel.json");
        assert!(read_panel(&path).is_none());
        std::fs::write(&path, r#"{"frase":"Hoy no hay nada en tu agenda."}"#).unwrap();
        assert_eq!(
            read_panel(&path).unwrap()["frase"],
            "Hoy no hay nada en tu agenda."
        );
        std::fs::write(&path, "[1,2]").unwrap();
        assert!(read_panel(&path).is_none());
        std::fs::write(&path, "{roto").unwrap();
        assert!(read_panel(&path).is_none());
        std::fs::write(&path, vec![b' '; (MAX_PANEL_BYTES + 1) as usize]).unwrap();
        assert!(read_panel(&path).is_none());
    }
}
