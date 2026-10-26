//! `/menu`: jump between conversations from the chat app with buttons.
//!
//! Lists the most recent conversations across every folder as inline buttons,
//! plus "new". Buttons carry `nav:` callback data, which the dispatcher turns
//! back into the equivalent command (`/resume N`, `/new`, `/menu`) before
//! routing — so a tap behaves exactly like typing that command. A successful
//! `/resume` answers with "exit" / "other conversations" buttons. The menu
//! also carries an auto-approve toggle (`nav:auto:on|off` → `/auto on|off`).

use sea_orm::DatabaseConnection;

use super::i18n::Lang;
use super::types::{ButtonStyle, InteractiveMessage, MessageButton, RichMessage};
use crate::db::service::{conversation_service, folder_service, sender_context_service};

/// How many recent conversations the menu offers.
pub const MENU_LIMIT: usize = 8;
const TITLE_MAX_CHARS: usize = 28;

/// Map a `nav:` callback payload to the command it stands for (without prefix).
pub fn nav_command(data: &str) -> Option<String> {
    let rest = data.strip_prefix("nav:")?;
    match rest {
        "new" => Some("new".to_string()),
        "menu" => Some("menu".to_string()),
        "auto:on" => Some("auto on".to_string()),
        "auto:off" => Some("auto off".to_string()),
        _ => {
            let id: i32 = rest.strip_prefix("resume:")?.parse().ok()?;
            Some(format!("resume {id}"))
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

fn menu_title(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "Conversaciones",
        _ => "Conversations",
    }
}

fn menu_hint(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "Toca una para seguir ahí, o empieza una nueva.",
        _ => "Tap one to continue there, or start a new one.",
    }
}

fn menu_empty(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "No hay conversaciones todavía.",
        _ => "No conversations yet.",
    }
}

fn new_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "➕ Nueva",
        _ => "➕ New",
    }
}

fn exit_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "🚪 Salir",
        _ => "🚪 Exit",
    }
}

fn others_label(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "📋 Otras",
        _ => "📋 Others",
    }
}

fn auto_toggle_label(enabled: bool, lang: Lang) -> &'static str {
    match (lang, enabled) {
        (Lang::Es, true) => "🤖 Permisos: automático (tocar para apagar)",
        (Lang::Es, false) => "✋ Permisos: preguntar (tocar para automático)",
        (_, true) => "🤖 Permissions: auto (tap to turn off)",
        (_, false) => "✋ Permissions: ask (tap for auto)",
    }
}

fn auto_state_text(enabled: bool, lang: Lang) -> &'static str {
    match (lang, enabled) {
        (Lang::Es, true) => {
            "Permisos automáticos activados: lo que pida permiso en este chat se aprueba solo."
        }
        (Lang::Es, false) => "Permisos automáticos apagados: cada permiso te llega para aprobar.",
        (_, true) => {
            "Auto-approve is on: permission requests in this chat are approved automatically."
        }
        (_, false) => "Auto-approve is off: every permission request comes to you.",
    }
}

fn auto_title(lang: Lang) -> &'static str {
    match lang {
        Lang::Es => "Permisos",
        _ => "Permissions",
    }
}

/// The button that flips auto-approve from its current state.
fn auto_toggle_button(enabled: bool, lang: Lang) -> MessageButton {
    let next = if enabled { "off" } else { "on" };
    nav_button(format!("nav:auto:{next}"), auto_toggle_label(enabled, lang))
}

async fn auto_approve_of(db: &DatabaseConnection, channel_id: i32, sender_id: &str) -> bool {
    sender_context_service::get_or_create(db, channel_id, sender_id)
        .await
        .map(|c| c.auto_approve)
        .unwrap_or(false)
}

/// `/auto [on|off]`: set or show auto-approve for this sender, with a toggle
/// button. Any other argument just reports the current state.
pub async fn handle_auto(
    db: &DatabaseConnection,
    channel_id: i32,
    sender_id: &str,
    args: &str,
    lang: Lang,
) -> InteractiveMessage {
    let wanted = match args.trim().to_ascii_lowercase().as_str() {
        "on" | "si" | "sí" | "yes" | "true" | "1" => Some(true),
        "off" | "no" | "false" | "0" => Some(false),
        _ => None,
    };
    let enabled = match wanted {
        Some(v) => {
            match sender_context_service::update_auto_approve(db, channel_id, sender_id, v).await {
                Ok(c) => c.auto_approve,
                Err(_) => auto_approve_of(db, channel_id, sender_id).await,
            }
        }
        None => auto_approve_of(db, channel_id, sender_id).await,
    };
    InteractiveMessage {
        base: RichMessage::info(auto_state_text(enabled, lang)).with_title(auto_title(lang)),
        buttons: vec![
            auto_toggle_button(enabled, lang),
            nav_button("nav:menu".to_string(), others_label(lang)),
        ],
        callback_context: serde_json::json!({ "kind": "nav" }),
    }
}

fn nav_button(id: String, label: impl Into<String>) -> MessageButton {
    MessageButton {
        id,
        label: label.into(),
        style: ButtonStyle::Default,
    }
}

/// `/menu`: recent conversations of every folder, newest first, plus "new".
pub async fn handle_menu(
    db: &DatabaseConnection,
    channel_id: i32,
    sender_id: &str,
    lang: Lang,
) -> InteractiveMessage {
    let ctx = sender_context_service::get_or_create(db, channel_id, sender_id)
        .await
        .ok();
    let current = ctx.as_ref().and_then(|c| c.current_conversation_id);
    let auto_approve = ctx.map(|c| c.auto_approve).unwrap_or(false);
    let convs = conversation_service::list_all(db, None, None, None, None, None, false)
        .await
        .unwrap_or_default();

    let mut buttons = Vec::new();
    for c in convs.iter().take(MENU_LIMIT) {
        let folder = folder_service::get_folder_by_id(db, c.folder_id)
            .await
            .ok()
            .flatten();
        let folder_label = folder
            .map(|f| f.alias.filter(|a| !a.trim().is_empty()).unwrap_or(f.name))
            .unwrap_or_default();
        let title = truncate_chars(
            c.title.as_deref().unwrap_or("(sin título)"),
            TITLE_MAX_CHARS,
        );
        let marker = if current == Some(c.id) { "● " } else { "" };
        buttons.push(nav_button(
            format!("nav:resume:{}", c.id),
            format!("{marker}{folder_label} · {title}"),
        ));
    }
    let body = if buttons.is_empty() {
        menu_empty(lang)
    } else {
        menu_hint(lang)
    };
    buttons.push(nav_button("nav:new".to_string(), new_label(lang)));
    buttons.push(auto_toggle_button(auto_approve, lang));

    InteractiveMessage {
        base: RichMessage::info(body).with_title(menu_title(lang)),
        buttons,
        callback_context: serde_json::json!({ "kind": "nav" }),
    }
}

/// Attach "exit" / "other conversations" buttons to a `/resume` answer.
pub fn with_nav_buttons(message: RichMessage, lang: Lang) -> InteractiveMessage {
    InteractiveMessage {
        base: message,
        buttons: vec![
            nav_button("nav:new".to_string(), exit_label(lang)),
            nav_button("nav:menu".to_string(), others_label(lang)),
        ],
        callback_context: serde_json::json!({ "kind": "nav" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::service::chat_channel_service;
    use crate::db::test_helpers::{fresh_in_memory_db, seed_folder};
    use crate::models::agent::AgentType;

    #[test]
    fn nav_command_maps_payloads() {
        assert_eq!(nav_command("nav:resume:95").as_deref(), Some("resume 95"));
        assert_eq!(nav_command("nav:new").as_deref(), Some("new"));
        assert_eq!(nav_command("nav:menu").as_deref(), Some("menu"));
        assert_eq!(nav_command("nav:auto:on").as_deref(), Some("auto on"));
        assert_eq!(nav_command("nav:auto:off").as_deref(), Some("auto off"));
        assert_eq!(nav_command("nav:auto:maybe"), None);
        assert_eq!(nav_command("nav:resume:abc"), None);
        assert_eq!(nav_command("nav:resume:1;rm"), None);
        assert_eq!(nav_command("cfg:folder:3"), None);
    }

    #[test]
    fn long_titles_are_truncated() {
        let t = truncate_chars("Index nexus servidor expansión arquitectura", 28);
        assert_eq!(t.chars().count(), 28);
        assert!(t.ends_with('…'));
        assert_eq!(truncate_chars("corto", 28), "corto");
    }

    #[tokio::test]
    async fn menu_lists_conversations_of_all_folders_and_new_last() {
        let db = fresh_in_memory_db().await;
        let f1 = seed_folder(&db, "/tmp/codeg-menu-a").await;
        let f2 = seed_folder(&db, "/tmp/codeg-menu-b").await;
        let a = conversation_service::create(
            &db.conn,
            f1,
            AgentType::ClaudeCode,
            Some("Uno".into()),
            None,
        )
        .await
        .unwrap();
        let b = conversation_service::create(
            &db.conn,
            f2,
            AgentType::OpenCode,
            Some("Dos".into()),
            None,
        )
        .await
        .unwrap();

        let msg = handle_menu(&db.conn, 1, "s", Lang::Es).await;
        let ids: Vec<&str> = msg.buttons.iter().map(|b| b.id.as_str()).collect();
        assert!(ids.contains(&format!("nav:resume:{}", a.id).as_str()));
        assert!(ids.contains(&format!("nav:resume:{}", b.id).as_str()));
        assert_eq!(ids[ids.len() - 2], "nav:new");
        assert_eq!(ids.last(), Some(&"nav:auto:on"));
        assert_eq!(msg.base.title.as_deref(), Some("Conversaciones"));
        assert!(msg.buttons.iter().all(|b| b.id.len() <= 64));
    }

    #[tokio::test]
    async fn menu_is_capped() {
        let db = fresh_in_memory_db().await;
        let f = seed_folder(&db, "/tmp/codeg-menu-cap").await;
        for i in 0..(MENU_LIMIT + 3) {
            conversation_service::create(
                &db.conn,
                f,
                AgentType::ClaudeCode,
                Some(format!("c{i}")),
                None,
            )
            .await
            .unwrap();
        }
        let msg = handle_menu(&db.conn, 1, "s", Lang::En).await;
        assert_eq!(msg.buttons.len(), MENU_LIMIT + 2);
    }

    #[tokio::test]
    async fn empty_menu_offers_new_and_the_auto_toggle() {
        let db = fresh_in_memory_db().await;
        let msg = handle_menu(&db.conn, 1, "s", Lang::Es).await;
        assert_eq!(msg.buttons.len(), 2);
        assert_eq!(msg.buttons[0].id, "nav:new");
        assert_eq!(msg.buttons[1].id, "nav:auto:on");
        assert_eq!(msg.base.body, "No hay conversaciones todavía.");
    }

    #[test]
    fn resume_answer_gets_exit_and_others() {
        let msg = with_nav_buttons(RichMessage::info("ok"), Lang::Es);
        let ids: Vec<&str> = msg.buttons.iter().map(|b| b.id.as_str()).collect();
        assert_eq!(ids, vec!["nav:new", "nav:menu"]);
        assert_eq!(msg.buttons[0].label, "🚪 Salir");
    }

    async fn seed_channel(db: &crate::db::AppDatabase) -> i32 {
        chat_channel_service::create(
            &db.conn,
            "Telegram test".to_string(),
            "telegram".to_string(),
            serde_json::json!({ "chat_id": "-100123" }).to_string(),
            true,
            false,
            None,
        )
        .await
        .expect("seed chat channel")
        .id
    }

    #[tokio::test]
    async fn auto_on_then_off_persists_and_flips_the_button() {
        let db = fresh_in_memory_db().await;
        let ch = seed_channel(&db).await;
        let on = handle_auto(&db.conn, ch, "s", "on", Lang::Es).await;
        assert!(on.base.body.starts_with("Permisos automáticos activados"));
        assert_eq!(on.buttons[0].id, "nav:auto:off");
        assert!(
            sender_context_service::get_or_create(&db.conn, ch, "s")
                .await
                .unwrap()
                .auto_approve
        );

        let menu = handle_menu(&db.conn, ch, "s", Lang::Es).await;
        assert_eq!(menu.buttons.last().unwrap().id, "nav:auto:off");

        let off = handle_auto(&db.conn, ch, "s", "OFF", Lang::Es).await;
        assert_eq!(off.buttons[0].id, "nav:auto:on");
        assert!(
            !sender_context_service::get_or_create(&db.conn, ch, "s")
                .await
                .unwrap()
                .auto_approve
        );
    }

    #[tokio::test]
    async fn auto_without_argument_only_reports() {
        let db = fresh_in_memory_db().await;
        let ch = seed_channel(&db).await;
        let msg = handle_auto(&db.conn, ch, "s", "", Lang::En).await;
        assert_eq!(msg.buttons[0].id, "nav:auto:on");
        assert!(
            !sender_context_service::get_or_create(&db.conn, ch, "s")
                .await
                .unwrap()
                .auto_approve
        );
    }

    #[tokio::test]
    async fn auto_is_per_sender() {
        let db = fresh_in_memory_db().await;
        let ch = seed_channel(&db).await;
        handle_auto(&db.conn, ch, "a", "on", Lang::Es).await;
        assert!(
            !sender_context_service::get_or_create(&db.conn, ch, "b")
                .await
                .unwrap()
                .auto_approve
        );
    }
}
