//! Compact chat mode (the default): a chat app is for quick, direct answers.
//!
//! Instead of one message per tool call plus a "responding…" status every few
//! seconds, the channel shows its native "typing…" indicator while the agent
//! works and then delivers only the answer — no "Turn Complete" title, no
//! Agent/Stop Reason fields, Markdown turned into plain text (the Telegram
//! backend escapes everything, so `**bold**` used to arrive with its asterisks)
//! and long answers split instead of cut in the middle. Permission requests,
//! questions and errors are untouched: those ask something of the user.
//!
//! A channel opts back into the detailed stream with `"verbose": true` in its
//! `config_json`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sea_orm::DatabaseConnection;

use super::i18n::Lang;
use crate::db::service::chat_channel_service;

/// Telegram keeps "typing…" visible for ~5 s; refresh a little before that.
const TYPING_EVERY: Duration = Duration::from_secs(4);
/// Below Telegram's 4096-character message cap, with room for escaping.
pub const MAX_CHUNK: usize = 3500;

static LAST_TYPING: LazyLock<Mutex<HashMap<String, Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `true` unless the channel's config asks for `"verbose": true`.
pub fn is_compact_config(config_json: &str) -> bool {
    !serde_json::from_str::<serde_json::Value>(config_json)
        .ok()
        .and_then(|v| v.get("verbose").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

pub async fn is_compact(db: &DatabaseConnection, channel_id: i32) -> bool {
    match chat_channel_service::get_by_id(db, channel_id).await {
        Ok(Some(ch)) => is_compact_config(&ch.config_json),
        // Unknown channel: keep the original detailed behavior.
        _ => false,
    }
}

/// Throttle for the typing indicator, per agent connection.
pub fn should_send_typing(connection_id: &str) -> bool {
    let now = Instant::now();
    let mut map = LAST_TYPING.lock().unwrap_or_else(|e| e.into_inner());
    if map.len() > 256 {
        map.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
    }
    match map.get(connection_id) {
        Some(last) if now.duration_since(*last) < TYPING_EVERY => false,
        _ => {
            map.insert(connection_id.to_string(), now);
            true
        }
    }
}

/// Markdown → plain chat text: no emphasis markers, no heading hashes,
/// bullets as "•", table rules dropped. Code stays as-is inside its fence.
pub fn plain_text(markdown: &str) -> String {
    let mut out = Vec::new();
    let mut in_code = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push(line.to_string());
            continue;
        }
        // Table separator rows like |---|:--:|
        if trimmed.starts_with('|') && trimmed.chars().all(|c| "|-: ".contains(c)) {
            continue;
        }
        let mut l = trimmed.trim_start_matches('#').trim_start().to_string();
        if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
            l = format!("• {}", &trimmed[2..]);
        }
        let l = l.replace("**", "").replace("__", "").replace('`', "");
        let l = if l.starts_with('|') {
            l.trim_matches('|')
                .split('|')
                .map(str::trim)
                .collect::<Vec<_>>()
                .join(" · ")
        } else {
            l
        };
        out.push(l);
    }
    let text = out.join("\n");
    // Collapse runs of blank lines left by removed rows/fences.
    let mut collapsed = String::new();
    let mut blank = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        collapsed.push_str(line);
        collapsed.push('\n');
    }
    collapsed.trim().to_string()
}

/// The whole answer, split at paragraph/line boundaries into chat-sized parts.
pub fn split_message(text: &str, max: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > max && !cur.is_empty() {
            parts.push(cur.trim_end().to_string());
            cur.clear();
        }
        if line.chars().count() > max {
            // A single huge line: hard-cut on char boundaries.
            let chars: Vec<char> = line.chars().collect();
            for chunk in chars.chunks(max) {
                parts.push(chunk.iter().collect::<String>().trim_end().to_string());
            }
            continue;
        }
        cur.push_str(line);
    }
    if !cur.trim().is_empty() {
        parts.push(cur.trim_end().to_string());
    }
    parts
}

/// What the user sees when the turn ends: just the answer, plus one short
/// line only when the turn did not finish normally.
pub fn final_reply(content: &str, stop_reason: &str, lang: Lang) -> String {
    let body = plain_text(content);
    let body = if body.is_empty() {
        match lang {
            Lang::Es => "Listo.".to_string(),
            _ => "Done.".to_string(),
        }
    } else {
        body
    };
    if stop_reason == "end_turn" {
        return body;
    }
    let note = match (lang, stop_reason) {
        (Lang::Es, "cancelled") => "⏹ Cancelado.",
        (Lang::Es, "max_tokens") => "⚠️ Se cortó por longitud.",
        (Lang::Es, _) => "⚠️ El turno no terminó normalmente.",
        (_, "cancelled") => "⏹ Cancelled.",
        (_, "max_tokens") => "⚠️ Cut off by length.",
        _ => "⚠️ The turn did not finish normally.",
    };
    format!("{body}\n\n{note}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_is_the_default_and_verbose_opts_out() {
        assert!(is_compact_config(r#"{"chat_id": "1"}"#));
        assert!(is_compact_config("not json"));
        assert!(!is_compact_config(r#"{"verbose": true}"#));
    }

    #[test]
    fn plain_text_drops_markdown_noise() {
        let md = "## Resumen\n\n**Listo**: el `test` pasa.\n- uno\n- dos\n\n| A | B |\n|---|---|\n| 1 | 2 |";
        assert_eq!(
            plain_text(md),
            "Resumen\n\nListo: el test pasa.\n• uno\n• dos\n\nA · B\n1 · 2"
        );
    }

    #[test]
    fn code_blocks_keep_their_content() {
        assert_eq!(
            plain_text("Mira:\n```py\nx = **y**\n```\nFin"),
            "Mira:\nx = **y**\nFin"
        );
    }

    #[test]
    fn long_answers_are_split_not_cut() {
        let text = (0..400).map(|i| format!("línea {i}\n")).collect::<String>();
        let parts = split_message(&text, 500);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.chars().count() <= 500));
        assert_eq!(parts.join("\n").lines().count(), 400);
    }

    #[test]
    fn final_reply_is_just_the_answer() {
        assert_eq!(final_reply("**Hola**", "end_turn", Lang::Es), "Hola");
        assert_eq!(final_reply("", "end_turn", Lang::Es), "Listo.");
        assert!(final_reply("x", "cancelled", Lang::Es).ends_with("⏹ Cancelado."));
    }

    #[test]
    fn typing_is_throttled_per_connection() {
        assert!(should_send_typing("conn-test-a"));
        assert!(!should_send_typing("conn-test-a"));
        assert!(should_send_typing("conn-test-b"));
    }
}
