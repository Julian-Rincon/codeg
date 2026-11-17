//! Microphone for Phantom's own pages (voice mode).
//!
//! WebKitGTK answers every `getUserMedia` with "denied" unless the host
//! enables media streams and grants the permission request itself; there is
//! no browser prompt to fall back on. Only the app's own origin is granted:
//! a window showing any other page keeps the default refusal.

/// Origins the app's own pages load from inside the desktop webview.
const APP_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

/// Whether a page at `uri` is one of Phantom's own pages.
pub fn es_de_la_app(uri: &str) -> bool {
    APP_ORIGINS
        .iter()
        .any(|o| uri == *o || uri.starts_with(&format!("{o}/")))
}

#[cfg(all(feature = "tauri-runtime", target_os = "linux"))]
pub fn permitir(window: &tauri::WebviewWindow) {
    let _ = window.with_webview(|wv| {
        use gtk::glib::object::Cast;
        use webkit2gtk::{
            PermissionRequestExt, SettingsExt, UserMediaPermissionRequest, WebViewExt,
        };
        let view = wv.inner();
        if let Some(settings) = WebViewExt::settings(&view) {
            settings.set_enable_media_stream(true);
        }
        view.connect_permission_request(|view, req| {
            if req.downcast_ref::<UserMediaPermissionRequest>().is_none() {
                return false; // not ours to decide: WebKit's default stands
            }
            if view.uri().is_some_and(|u| es_de_la_app(u.as_str())) {
                req.allow();
            } else {
                req.deny();
            }
            true
        });
    });
}

#[cfg(all(feature = "tauri-runtime", not(target_os = "linux")))]
pub fn permitir(_window: &tauri::WebviewWindow) {}

#[cfg(test)]
mod tests {
    use super::es_de_la_app;

    #[test]
    fn solo_las_paginas_de_la_app() {
        assert!(es_de_la_app("tauri://localhost"));
        assert!(es_de_la_app("tauri://localhost/workspace?x=1"));
        assert!(es_de_la_app("http://tauri.localhost/"));
        assert!(!es_de_la_app("https://example.com/"));
        assert!(!es_de_la_app("tauri://localhost.evil.com/"));
        assert!(!es_de_la_app("http://127.0.0.1:3080/"));
    }
}
