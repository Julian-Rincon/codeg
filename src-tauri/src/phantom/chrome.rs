//! Claude in Chrome for Claude Code sessions opened by Phantom (NEXUS too).
//!
//! The CLI turns the extension on by itself only in interactive mode; through
//! the ACP adapter it needs `--chrome`, passed as an extra CLI argument in the
//! session's `_meta.claudeCode.options.extraArgs`. Phantom follows the same
//! switch the CLI uses (`claudeInChromeDefaultEnabled` in `~/.claude.json`),
//! and `PHANTOM_CLAUDE_CHROME=0|1` overrides it.

use serde_json::{Map, Value};

fn activo_por(env: Option<&str>, claude_json: Option<&str>) -> bool {
    match env {
        Some("1") => return true,
        Some("0") => return false,
        _ => {}
    }
    claude_json
        .and_then(|t| serde_json::from_str::<Value>(t).ok())
        .and_then(|v| v.get("claudeInChromeDefaultEnabled")?.as_bool())
        .unwrap_or(false)
}

fn activo() -> bool {
    if cfg!(test) {
        return false; // the tests pin the exact session meta
    }
    let env = std::env::var("PHANTOM_CLAUDE_CHROME").ok();
    let claude_json =
        dirs::home_dir().and_then(|h| std::fs::read_to_string(h.join(".claude.json")).ok());
    activo_por(env.as_deref(), claude_json.as_deref())
}

fn poner(options: &mut Map<String, Value>) {
    let extra = options
        .entry("extraArgs")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Value::Object(extra) = extra {
        extra.insert("chrome".to_string(), Value::String(String::new()));
    }
}

/// Adds `--chrome` to a Claude Code session's options when it is on.
pub fn agregar(options: &mut Map<String, Value>) {
    if activo() {
        poner(options);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigue_al_ajuste_de_claude() {
        let on = r#"{"claudeInChromeDefaultEnabled": true}"#;
        let off = r#"{"claudeInChromeDefaultEnabled": false}"#;
        assert!(activo_por(None, Some(on)));
        assert!(!activo_por(None, Some(off)));
        assert!(!activo_por(None, None));
        assert!(!activo_por(Some("0"), Some(on)));
        assert!(activo_por(Some("1"), Some(off)));
    }

    #[test]
    fn pone_la_bandera_sin_tocar_lo_demas() {
        let mut o = Map::new();
        o.insert("extraArgs".into(), serde_json::json!({"x": "1"}));
        poner(&mut o);
        assert_eq!(o["extraArgs"], serde_json::json!({"x": "1", "chrome": ""}));
    }
}
