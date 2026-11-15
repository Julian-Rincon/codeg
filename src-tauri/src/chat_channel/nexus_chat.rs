//! NEXUS: el chat general de Telegram se comporta como un asistente, no como un bot.
//!
//! Dos cosas, las dos solo para el canal de chat general:
//! - los acuses de recibo ("Tarea iniciada [Claude Code] #302 @ espacio", "Mensaje
//!   enviado.") no se mandan: en su lugar aparece el "escribiendo…" nativo;
//! - los atajos del menú del bot (`/hoy`, `/tareas`, …) no son comandos de Phantom:
//!   se convierten en la frase que Julian le diría al agente.
//!
//! Vive en un archivo propio para que las fusiones de upstream solo tengan que
//! conservar dos llamadas en `command_dispatcher.rs`.

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
}
