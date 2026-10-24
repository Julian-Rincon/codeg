//! Phantom: the desktop app replaces `codeg-server` on this machine, so its
//! embedded web server must bind and serve like the server does — from the
//! same `CODEG_HOST` / `CODEG_STATIC_DIR` the systemd unit sets — instead of
//! upstream's `0.0.0.0` and the bundle's resource dir.

// Only the desktop build calls these; the server binds via its own config.
#![cfg_attr(not(feature = "tauri-runtime"), allow(dead_code))]

use std::path::PathBuf;

/// The bind host from `CODEG_HOST` (`127.0.0.1` in Phantom's unit); `None`
/// keeps upstream's default.
pub(crate) fn host_from_env() -> Option<String> {
    host_from(std::env::var("CODEG_HOST").ok())
}

fn host_from(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// The web build from `CODEG_STATIC_DIR`, if it holds an `index.html`.
pub(crate) fn static_dir_from_env() -> Option<PathBuf> {
    static_dir_from(std::env::var("CODEG_STATIC_DIR").ok())
}

fn static_dir_from(value: Option<String>) -> Option<PathBuf> {
    let dir = PathBuf::from(value.filter(|v| !v.trim().is_empty())?);
    dir.join("index.html").exists().then_some(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_comes_from_codeg_host_and_blank_means_default() {
        assert_eq!(
            host_from(Some("127.0.0.1".into())),
            Some("127.0.0.1".into())
        );
        assert_eq!(
            host_from(Some(" 127.0.0.1 ".into())),
            Some("127.0.0.1".into())
        );
        assert_eq!(host_from(Some("  ".into())), None);
        assert_eq!(host_from(None), None);
    }

    #[test]
    fn static_dir_needs_an_index_html() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_string_lossy().to_string();
        assert_eq!(static_dir_from(Some(path.clone())), None);
        std::fs::write(dir.path().join("index.html"), "<html></html>").unwrap();
        assert_eq!(static_dir_from(Some(path)), Some(dir.path().to_path_buf()));
        assert_eq!(static_dir_from(Some(String::new())), None);
        assert_eq!(static_dir_from(None), None);
    }
}
