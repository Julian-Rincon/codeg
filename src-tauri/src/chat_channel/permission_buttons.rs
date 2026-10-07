//! Inline buttons for agent permission requests, so a blocked agent can be
//! approved or denied from the phone without typing `/approve`.
//!
//! Telegram caps `callback_data` at 64 bytes — too short for a connection id
//! plus a request id — so each request is registered here under a short random
//! token and its buttons carry `perm:<token>:<option index>`. A token resolves
//! at most once: a second tap (or a tap after the request expired) gets `None`.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::acp::types::PermissionOptionInfo;

use super::i18n::Lang;
use super::types::{ButtonStyle, InteractiveMessage, MessageButton, RichMessage};

const PREFIX: &str = "perm:";
const TTL: Duration = Duration::from_secs(24 * 3600);

struct Entry {
    connection_id: String,
    request_id: String,
    options: Vec<PermissionOptionInfo>,
    tool_description: String,
    created: Instant,
}

static PENDING: LazyLock<Mutex<HashMap<String, Entry>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
pub struct Resolved {
    pub connection_id: String,
    pub request_id: String,
    pub option: PermissionOptionInfo,
    pub tool_description: String,
}

impl Resolved {
    pub fn approved(&self) -> bool {
        is_allow(&self.option.kind)
    }
}

fn is_allow(kind: &str) -> bool {
    kind.starts_with("allow")
}

fn is_reject(kind: &str) -> bool {
    kind.starts_with("reject") || kind == "deny"
}

/// Register a pending permission request and return its short token.
pub fn register(
    connection_id: &str,
    request_id: &str,
    options: &[PermissionOptionInfo],
    tool_description: &str,
) -> String {
    let token = uuid::Uuid::new_v4().simple().to_string()[..10].to_string();
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    pending.retain(|_, e| e.created.elapsed() < TTL);
    pending.insert(
        token.clone(),
        Entry {
            connection_id: connection_id.to_string(),
            request_id: request_id.to_string(),
            options: options.to_vec(),
            tool_description: tool_description.to_string(),
            created: Instant::now(),
        },
    );
    token
}

pub fn is_permission_callback(data: &str) -> bool {
    data.starts_with(PREFIX)
}

/// Resolve a button tap. Single use: the token is consumed even if the index
/// is out of range, so a stale keyboard can never answer twice.
pub fn take(data: &str) -> Option<Resolved> {
    let rest = data.strip_prefix(PREFIX)?;
    let (token, index) = rest.rsplit_once(':')?;
    let index: usize = index.parse().ok()?;
    let entry = PENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(token)?;
    if entry.created.elapsed() >= TTL {
        return None;
    }
    let option = entry.options.get(index)?.clone();
    Some(Resolved {
        connection_id: entry.connection_id,
        request_id: entry.request_id,
        option,
        tool_description: entry.tool_description,
    })
}

fn label(option: &PermissionOptionInfo, lang: Lang) -> String {
    let es = lang == Lang::Es;
    match option.kind.as_str() {
        "allow_once" => if es { "✅ Aprobar" } else { "✅ Approve" }.to_string(),
        "allow_always" => if es { "✅ Siempre" } else { "✅ Always" }.to_string(),
        "reject_once" => if es { "❌ Rechazar" } else { "❌ Deny" }.to_string(),
        "reject_always" => if es { "❌ Nunca" } else { "❌ Never" }.to_string(),
        _ => option.name.clone(),
    }
}

/// One button per option the agent offered (the same choices Phantom shows),
/// in the agent's order.
pub fn with_buttons(
    base: RichMessage,
    token: &str,
    options: &[PermissionOptionInfo],
    lang: Lang,
) -> InteractiveMessage {
    let buttons = options
        .iter()
        .enumerate()
        .map(|(i, o)| MessageButton {
            id: format!("{PREFIX}{token}:{i}"),
            label: label(o, lang),
            style: if is_reject(&o.kind) {
                ButtonStyle::Danger
            } else if is_allow(&o.kind) {
                ButtonStyle::Primary
            } else {
                ButtonStyle::Default
            },
        })
        .collect();
    InteractiveMessage {
        base,
        buttons,
        callback_context: serde_json::json!({ "kind": "permission" }),
    }
}

/// The option `/approve` or `/deny` picks. Approving prefers a one-time grant
/// (ACP `allow_once`) over a standing rule; the legacy kinds are kept for
/// agents that still send them.
pub fn pick_option(
    options: &[PermissionOptionInfo],
    approve: bool,
) -> Option<&PermissionOptionInfo> {
    let by_kind = |kinds: &[&str]| {
        kinds
            .iter()
            .find_map(|k| options.iter().find(|o| o.kind == *k))
    };
    if approve {
        by_kind(&["allow_once", "allow", "allowForSession", "allow_always"])
            .or_else(|| options.first())
    } else {
        by_kind(&["reject_once", "deny", "reject_always"]).or_else(|| options.last())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opt(id: &str, kind: &str) -> PermissionOptionInfo {
        PermissionOptionInfo {
            option_id: id.into(),
            name: id.into(),
            kind: kind.into(),
            meta: None,
        }
    }

    fn claude_options() -> Vec<PermissionOptionInfo> {
        vec![
            opt("allow_always_id", "allow_always"),
            opt("allow_id", "allow_once"),
            opt("reject_id", "reject_once"),
        ]
    }

    #[test]
    fn callback_data_fits_telegram_64_byte_limit() {
        let options = claude_options();
        let token = register(
            "0b3f6a52-6c84-4b8f-9d0a-0d6f5c1a2b3c",
            "6f1c2d3e-4a5b-4c6d-8e7f-9a0b1c2d3e4f",
            &options,
            "Bash: ls",
        );
        let msg = with_buttons(RichMessage::info("x"), &token, &options, Lang::Es);
        assert_eq!(msg.buttons.len(), 3);
        assert!(msg.buttons.iter().all(|b| b.id.len() <= 64));
    }

    #[test]
    fn tap_resolves_the_chosen_option_once() {
        let options = claude_options();
        let token = register("conn", "req", &options, "Bash: ls");
        let data = format!("perm:{token}:1");
        let r = take(&data).expect("primer toque resuelve");
        assert_eq!(r.connection_id, "conn");
        assert_eq!(r.request_id, "req");
        assert_eq!(r.option.option_id, "allow_id");
        assert!(r.approved());
        assert!(
            take(&data).is_none(),
            "un segundo toque no vuelve a responder"
        );
    }

    #[test]
    fn other_button_of_an_answered_request_is_dead_too() {
        let options = claude_options();
        let token = register("conn", "req", &options, "x");
        assert!(take(&format!("perm:{token}:2")).is_some());
        assert!(take(&format!("perm:{token}:0")).is_none());
    }

    #[test]
    fn unknown_or_malformed_callbacks_resolve_to_nothing() {
        assert!(take("perm:nope:0").is_none());
        assert!(take("perm:").is_none());
        assert!(take("nav:menu").is_none());
        assert!(!is_permission_callback("nav:menu"));
        assert!(is_permission_callback("perm:abc:0"));
    }

    #[test]
    fn spanish_labels_and_styles_follow_option_kind() {
        let options = claude_options();
        let msg = with_buttons(RichMessage::info("x"), "t", &options, Lang::Es);
        let labels: Vec<_> = msg.buttons.iter().map(|b| b.label.as_str()).collect();
        assert_eq!(labels, ["✅ Siempre", "✅ Aprobar", "❌ Rechazar"]);
        assert_eq!(msg.buttons[2].style, ButtonStyle::Danger);
        assert_eq!(msg.buttons[1].style, ButtonStyle::Primary);
    }

    #[test]
    fn approve_prefers_a_one_time_grant_over_always() {
        // claude-agent-acp lists "always" first: /approve must not pick it.
        let options = claude_options();
        assert_eq!(pick_option(&options, true).unwrap().option_id, "allow_id");
        assert_eq!(pick_option(&options, false).unwrap().option_id, "reject_id");
    }

    #[test]
    fn legacy_kinds_still_work() {
        let options = vec![opt("a", "allow"), opt("d", "deny")];
        assert_eq!(pick_option(&options, true).unwrap().option_id, "a");
        assert_eq!(pick_option(&options, false).unwrap().option_id, "d");
    }
}
