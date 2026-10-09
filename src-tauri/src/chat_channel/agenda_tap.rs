//! NEXUS agenda buttons (`ag:` callbacks).
//!
//! The NEXUS meeting watcher posts calendar notices through this same bot. Its
//! buttons are stateless (the callback data carries the Google Calendar event
//! id), so Phantom only forwards the tap to the local `nexus agenda-tap` CLI,
//! which applies it with the PC's Google token and prints one status line.

use std::path::PathBuf;
use std::time::Duration;

const PREFIX: &str = "ag:";
const TIMEOUT: Duration = Duration::from_secs(30);

/// `ag:(ok|del|mv):<event id>[:<epoch>]` — same shape the Python side emits.
pub fn is_agenda_callback(data: &str) -> bool {
    let Some(rest) = data.strip_prefix(PREFIX) else {
        return false;
    };
    let mut parts = rest.split(':');
    let (Some(action), Some(id)) = (parts.next(), parts.next()) else {
        return false;
    };
    let epoch = parts.next();
    if parts.next().is_some() || !matches!(action, "ok" | "del" | "mv") {
        return false;
    }
    let id_ok = (1..=40).contains(&id.len())
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    let epoch_ok = match (action, epoch) {
        ("mv", Some(e)) => (9..=11).contains(&e.len()) && e.chars().all(|c| c.is_ascii_digit()),
        ("mv", None) => false,
        (_, None) => true,
        (_, Some(_)) => false,
    };
    id_ok && epoch_ok
}

/// `NEXUS_BIN`, else the NEXUS virtualenv entry point under `$HOME`.
fn nexus_bin() -> PathBuf {
    if let Some(bin) = std::env::var_os("NEXUS_BIN").filter(|v| !v.is_empty()) {
        return PathBuf::from(bin);
    }
    let home = std::env::var_os("HOME").unwrap_or_default();
    PathBuf::from(home).join("Documentos/Proyectos/nexus/.venv/bin/nexus")
}

/// First non-empty stdout line, or a short error line. Never panics.
pub async fn run(data: &str) -> String {
    let mut cmd = tokio::process::Command::new(nexus_bin());
    cmd.arg("agenda-tap")
        .arg(data)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) => status_line(&out.stdout)
            .unwrap_or_else(|| "Error: NEXUS no respondió nada".to_string()),
        Ok(Err(e)) => format!("Error: no pude ejecutar NEXUS ({e})"),
        Err(_) => "Error: NEXUS tardó demasiado".to_string(),
    }
}

fn status_line(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| l.chars().take(200).collect())
}

/// The tapped message keeps its text; the status goes underneath.
pub fn tapped_text(original: &str, status: &str) -> String {
    format!("{original}\n\n{status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_three_actions() {
        assert!(is_agenda_callback("ag:ok:abc123"));
        assert!(is_agenda_callback("ag:del:dkosoqhbg2mfoq45t7q57vma94"));
        assert!(is_agenda_callback("ag:mv:abc_1-2:1760389200"));
    }

    #[test]
    fn rejects_other_or_malformed_data() {
        for bad in [
            "perm:x:0",
            "q:x:1",
            "ag:",
            "ag:ok",
            "ag:rm:abc",
            "ag:ok:abc:123456789",
            "ag:mv:abc",
            "ag:mv:abc:12",
            "ag:ok:a b",
            "ag:ok:abc;rm -rf",
            "ag:del:abc:1:2",
        ] {
            assert!(!is_agenda_callback(bad), "{bad}");
        }
        assert!(!is_agenda_callback(&format!("ag:ok:{}", "a".repeat(41))));
    }

    #[test]
    fn status_line_takes_first_non_empty_line() {
        assert_eq!(status_line(b"\n  ok listo \nmas").as_deref(), Some("ok listo"));
        assert_eq!(status_line(b"   \n"), None);
    }

    #[test]
    fn tapped_text_keeps_the_original() {
        assert_eq!(tapped_text("hola", "✅ Agendado"), "hola\n\n✅ Agendado");
    }
}
