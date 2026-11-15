//! NEXUS: el chat general de Telegram se comporta como un asistente, no como un bot.
//!
//! Dos cosas, las dos solo para el canal de chat general:
//! - los acuses de recibo ("Tarea iniciada [Claude Code] #302 @ espacio", "Mensaje
//!   enviado.") no se mandan: en su lugar aparece el "escribiendo…" nativo;
//! - los atajos del menú del bot (`/hoy`, `/tareas`, …) no son comandos de Phantom:
//!   se convierten en la frase que Julian le diría al agente.
//!
//! - `/vozauto`, `/voz`, `/hibrido` y `/texto` eligen si responde con audio; en
//!   automático lo decide la agenda (`voz_franjas` del panel de NEXUS).
//!
//! Vive en un archivo propio para que las fusiones de upstream solo tengan que
//! conservar tres llamadas en `command_dispatcher.rs`.

use std::path::Path;

use super::i18n::{self, Lang};
use super::types::RichMessage;

/// ¿Es un acuse de recibo que en el chat general sobra?
pub fn is_ack(message: &RichMessage, lang: Lang) -> bool {
    match message.title.as_deref() {
        Some(title) => title == i18n::task_started_title(lang),
        None => message.fields.is_empty() && message.body == i18n::message_sent(lang),
    }
}

/// Frase para el agente si `text` es un atajo de NEXUS; `None` si no lo es.
pub fn shortcut(text: &str, prefix: &str) -> Option<String> {
    let rest = text.trim().strip_prefix(prefix)?;
    let (command, args) = match rest.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (rest, ""),
    };
    // En grupos Telegram manda `/hoy@nombre_del_bot`.
    let command = command.split('@').next().unwrap_or(command).to_lowercase();
    let phrase = match command.as_str() {
        "hoy" => "¿Qué tengo hoy? Agenda, lo que vence y lo que no debería dejar pasar.",
        "tareas" => "Muéstrame mis tareas abiertas, primero las vencidas.",
        "gym" => "¿Qué me toca entrenar hoy y con qué pesos?",
        "correo" => "¿Qué correos tengo esperando respuesta?",
        "briefing" => "Dame el briefing de este momento: agenda, tareas, correo y lo que viene.",
        "recuerda" if !args.is_empty() => return Some(format!("Guarda esto en tu memoria: {args}")),
        "recuerda" => "¿Qué es lo último que has aprendido de mí?",
        _ => return None,
    };
    Some(phrase.to_string())
}

/// Cómo responde NEXUS en el chat general: con audio siempre, solo cuando Julian
/// mandó audio (lo que Phantom hace de fábrica), o nunca.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceMode {
    Voice,
    Hybrid,
    Text,
}

fn voice_mode_path(data_dir: &Path) -> std::path::PathBuf {
    data_dir.join("nexus-voice-mode")
}

fn nexus_panel_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|home| home.join(".local/share/nexus/panel.json"))
}

fn mode_from_word(word: &str) -> Option<VoiceMode> {
    match word {
        "voz" => Some(VoiceMode::Voice),
        "hibrido" => Some(VoiceMode::Hybrid),
        "texto" => Some(VoiceMode::Text),
        _ => None,
    }
}

/// Modo automático: la franja de `voz_franjas` del panel de NEXUS en la que cae
/// `now` (texto durante un evento, voz en el trayecto entre dos). Fuera de toda
/// franja, o sin panel, híbrido. Las reglas viven en `nexus/voz_modo.py`.
fn auto_mode(panel: &Path, now: chrono::DateTime<chrono::Utc>) -> VoiceMode {
    let Ok(bytes) = std::fs::read(panel) else {
        return VoiceMode::Hybrid;
    };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return VoiceMode::Hybrid;
    };
    let parse = |v: &serde_json::Value| {
        chrono::DateTime::parse_from_rfc3339(v.as_str()?)
            .ok()
            .map(|d| d.with_timezone(&chrono::Utc))
    };
    value["voz_franjas"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|slot| {
            let (from, to) = (parse(&slot["desde"])?, parse(&slot["hasta"])?);
            if from <= now && now < to {
                mode_from_word(slot["modo"].as_str()?)
            } else {
                None
            }
        })
        .unwrap_or(VoiceMode::Hybrid)
}

/// El modo que aplica ahora: el que Julian fijó con `/voz`, `/hibrido` o `/texto`,
/// y si no fijó ninguno (o pidió `/vozauto`), el que dicta su agenda.
pub fn voice_mode(data_dir: &Path) -> VoiceMode {
    let stored = std::fs::read_to_string(voice_mode_path(data_dir)).unwrap_or_default();
    mode_from_word(stored.trim()).unwrap_or_else(|| match nexus_panel_path() {
        Some(panel) => auto_mode(&panel, chrono::Utc::now()),
        None => VoiceMode::Hybrid,
    })
}

/// `/vozauto`, `/voz`, `/hibrido`, `/texto`: guarda el modo y devuelve la confirmación.
/// `None` si `text` no es uno de esos comandos.
pub fn voice_mode_command(text: &str, prefix: &str, data_dir: &Path) -> Option<String> {
    let command = text.trim().strip_prefix(prefix)?;
    let command = command.split('@').next().unwrap_or(command).to_lowercase();
    let (stored, reply) = match command.as_str() {
        // `/auto` a secas ya es de Phantom (aprobar permisos solo).
        "vozauto" | "voz_auto" => (
            "auto",
            "Modo automático: texto mientras estás en una reunión o clase, voz cuando vas de una a otra, híbrido el resto del tiempo.",
        ),
        "voz" => ("voz", "Modo voz: te respondo siempre con audio."),
        "hibrido" | "híbrido" => (
            "hibrido",
            "Modo híbrido: te respondo con audio cuando me hablas y con texto cuando me escribes.",
        ),
        "texto" => ("texto", "Modo texto: solo te escribo, aunque me mandes audio."),
        _ => return None,
    };
    match std::fs::write(voice_mode_path(data_dir), stored) {
        Ok(()) => Some(reply.to_string()),
        Err(e) => Some(format!("No pude guardar el modo de voz: {e}")),
    }
}

/// El idioma de la respuesta hablada que corresponde al modo (`None` = solo texto).
pub fn apply_voice_mode(mode: VoiceMode, incoming: Option<String>) -> Option<String> {
    match mode {
        VoiceMode::Voice => incoming.or_else(|| Some("es".to_string())),
        VoiceMode::Hybrid => incoming,
        VoiceMode::Text => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_acks_are_recognised_and_real_answers_are_not() {
        let started = RichMessage::info("[Claude Code] #302 @ espacio")
            .with_title(i18n::task_started_title(Lang::Es));
        assert!(is_ack(&started, Lang::Es));
        assert!(is_ack(&RichMessage::info(i18n::message_sent(Lang::Es)), Lang::Es));
        assert!(!is_ack(&RichMessage::info("Mañana tienes parcial a las 7."), Lang::Es));
        assert!(!is_ack(
            &RichMessage::info("x").with_title(i18n::unknown_command_title(Lang::Es)),
            Lang::Es
        ));
    }

    #[test]
    fn menu_shortcuts_become_sentences_and_real_commands_are_left_alone() {
        assert!(shortcut("/hoy", "/").unwrap().contains("hoy"));
        assert!(shortcut("/HOY@nexus_bot", "/").is_some());
        assert_eq!(
            shortcut("/recuerda el parcial es el 20", "/").unwrap(),
            "Guarda esto en tu memoria: el parcial es el 20"
        );
        assert!(shortcut("/new", "/").is_none());
        assert!(shortcut("/sessions", "/").is_none());
        assert!(shortcut("hoy", "/").is_none());
        assert!(shortcut("qué tengo /hoy", "/").is_none());
    }

    #[test]
    fn voice_mode_is_stored_and_decides_the_spoken_reply() {
        let dir = tempfile::tempdir().unwrap();
        assert!(voice_mode_command("/hoy", "/", dir.path()).is_none());
        assert!(voice_mode_command("/voz", "/", dir.path()).unwrap().contains("audio"));
        assert_eq!(voice_mode(dir.path()), VoiceMode::Voice);
        voice_mode_command("/TEXTO@nexus_bot", "/", dir.path()).unwrap();
        assert_eq!(voice_mode(dir.path()), VoiceMode::Text);
        voice_mode_command("/híbrido", "/", dir.path()).unwrap();
        assert_eq!(voice_mode(dir.path()), VoiceMode::Hybrid);

        let spoke = Some("es".to_string());
        assert_eq!(apply_voice_mode(VoiceMode::Voice, None), Some("es".to_string()));
        assert_eq!(apply_voice_mode(VoiceMode::Hybrid, None), None);
        assert_eq!(apply_voice_mode(VoiceMode::Hybrid, spoke.clone()), spoke);
        assert_eq!(apply_voice_mode(VoiceMode::Text, spoke), None);
    }

    #[test]
    fn auto_mode_follows_the_agenda_slots_and_falls_back_to_hybrid() {
        let dir = tempfile::tempdir().unwrap();
        let panel = dir.path().join("panel.json");
        let at = |t: &str| {
            chrono::DateTime::parse_from_rfc3339(t)
                .unwrap()
                .with_timezone(&chrono::Utc)
        };
        assert_eq!(auto_mode(&panel, at("2026-11-17T08:00:00-05:00")), VoiceMode::Hybrid);
        std::fs::write(
            &panel,
            r#"{"voz_franjas":[
                {"desde":"2026-11-17T07:00:00-05:00","hasta":"2026-11-17T09:00:00-05:00","modo":"texto"},
                {"desde":"2026-11-17T09:00:00-05:00","hasta":"2026-11-17T09:30:00-05:00","modo":"voz"},
                {"desde":"mal","hasta":"2026-11-17T23:00:00-05:00","modo":"voz"}]}"#,
        )
        .unwrap();
        assert_eq!(auto_mode(&panel, at("2026-11-17T08:00:00-05:00")), VoiceMode::Text);
        assert_eq!(auto_mode(&panel, at("2026-11-17T09:00:00-05:00")), VoiceMode::Voice);
        assert_eq!(auto_mode(&panel, at("2026-11-17T09:30:00-05:00")), VoiceMode::Hybrid);
        assert_eq!(auto_mode(&panel, at("2026-11-18T08:00:00-05:00")), VoiceMode::Hybrid);
        std::fs::write(&panel, "{roto").unwrap();
        assert_eq!(auto_mode(&panel, at("2026-11-17T08:00:00-05:00")), VoiceMode::Hybrid);

        // Un modo fijado a mano manda sobre la agenda; `/vozauto` lo suelta.
        voice_mode_command("/texto", "/", dir.path()).unwrap();
        assert_eq!(voice_mode(dir.path()), VoiceMode::Text);
        assert!(voice_mode_command("/auto", "/", dir.path()).is_none());
        assert!(voice_mode_command("/vozauto", "/", dir.path()).unwrap().contains("automático"));
        assert_eq!(std::fs::read_to_string(voice_mode_path(dir.path())).unwrap(), "auto");
    }
}
