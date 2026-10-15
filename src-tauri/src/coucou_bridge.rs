//! Bridge from Phantom to Coucou (the agent island, github.com/louis-cfm/coucou).
//!
//! The agents Phantom hosts already report to Coucou on their own: Claude Code
//! through its hooks, OpenCode and Hermes through Coucou's plugins. What only
//! Phantom knows is the orchestration around them — delegations, automations
//! (the NEXUS briefing) and a non-Claude agent blocked on an approval card that
//! Coucou cannot answer. This task turns those into hook events on a dedicated
//! `phantom` pill, so the island (and the paired Android phone) shows them.
//!
//! Transport: newline-terminated JSON on `$XDG_RUNTIME_DIR/coucou.sock`, one
//! connection per event, as Coucou's own relay does. When Coucou is not
//! running the connect fails and the event is dropped — Phantom never waits on
//! it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use crate::acp::types::{AcpEvent, DelegationResultSummary};
use crate::acp::InternalEventBus;
use crate::web::event_bridge::{WebEventBroadcaster, AUTOMATION_CHANGED_EVENT};

/// Pill name; must match Coucou's `^[a-z0-9-]{1,24}$`.
pub const AGENT: &str = "phantom";
const SESSION_ID: &str = "phantom";
const PREVIEW_CHARS: usize = 60;

/// Work Phantom has in flight, so the pill only settles when all of it is done.
#[derive(Debug, Default)]
pub struct BridgeState {
    /// child connection id → agent label.
    delegations: HashMap<String, String>,
    /// automation run ids.
    runs: Vec<i32>,
    /// permission request id → agent connection id.
    permissions: HashMap<String, String>,
    session_started: bool,
}

impl BridgeState {
    fn busy(&self) -> bool {
        !self.delegations.is_empty() || !self.runs.is_empty()
    }

    fn event(&mut self, name: &str, extra: Value) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.session_started {
            self.session_started = true;
            out.push(base("SessionStart"));
        }
        let mut ev = base(name);
        if let (Some(obj), Value::Object(more)) = (ev.as_object_mut(), extra) {
            obj.extend(more);
        }
        out.push(ev);
        out
    }

    fn settle(&mut self, mut out: Vec<Value>, summary: &str) -> Vec<Value> {
        if !self.busy() && self.permissions.is_empty() {
            out.extend(self.event("Stop", json!({ "last_assistant_message": summary })));
        }
        out
    }

    /// Hook events for one ACP envelope (`connection_id` is the envelope's).
    pub fn on_acp(&mut self, connection_id: &str, event: &AcpEvent) -> Vec<Value> {
        match event {
            AcpEvent::DelegationStarted {
                child_connection_id,
                agent_type,
                task_preview,
                ..
            } => {
                let agent = agent_type.to_string();
                self.delegations
                    .insert(child_connection_id.clone(), agent.clone());
                self.event(
                    "PreToolUse",
                    json!({
                        "tool_name": format!("Delegar → {agent}"),
                        "tool_input": { "query": preview(task_preview) },
                    }),
                )
            }
            AcpEvent::DelegationCompleted {
                child_connection_id,
                agent_type,
                result,
                ..
            } => {
                if self.delegations.remove(child_connection_id).is_none() {
                    return Vec::new();
                }
                let agent = agent_type.to_string();
                let (name, summary) = match result {
                    DelegationResultSummary::Ok { text_preview, .. } => (
                        "PostToolUse",
                        match text_preview {
                            Some(t) if !t.trim().is_empty() => {
                                format!("{agent}: {}", preview(t))
                            }
                            _ => format!("{agent} terminó"),
                        },
                    ),
                    DelegationResultSummary::Err { error_code } => {
                        ("PostToolUseFailure", format!("{agent} falló ({error_code})"))
                    }
                };
                let out = self.event(
                    name,
                    json!({ "tool_name": format!("Delegar → {agent}") }),
                );
                self.settle(out, &summary)
            }
            AcpEvent::PermissionRequest {
                request_id,
                tool_call,
                ..
            } => {
                self.permissions
                    .insert(request_id.clone(), connection_id.to_string());
                let what = tool_call
                    .get("title")
                    .and_then(Value::as_str)
                    .map(preview)
                    .unwrap_or_else(|| "una acción".to_string());
                // Coucou turns a Notification ending in "?" into its question
                // state, which the phone shows too.
                self.event(
                    "Notification",
                    json!({ "message": format!("Phantom espera tu aprobación: {what}?") }),
                )
            }
            AcpEvent::PermissionResolved { request_id } => {
                if self.permissions.remove(request_id).is_none() {
                    return Vec::new();
                }
                let out = self.event("PostToolUse", json!({ "tool_name": "Aprobación" }));
                self.settle(out, "Aprobación respondida")
            }
            // A dead connection can't be answered: forget its approvals (and a
            // delegation child it was) so the pill doesn't stay stuck.
            AcpEvent::StatusChanged {
                status: crate::acp::types::ConnectionStatus::Disconnected,
            } => {
                let before = self.permissions.len() + self.delegations.len();
                self.permissions.retain(|_, c| c != connection_id);
                self.delegations.remove(connection_id);
                if self.permissions.len() + self.delegations.len() == before {
                    return Vec::new();
                }
                self.settle(Vec::new(), "Sesión cerrada")
            }
            _ => Vec::new(),
        }
    }

    /// Hook events for one side-channel broadcast (automation runs).
    pub fn on_web(&mut self, channel: &str, payload: &Value) -> Vec<Value> {
        if channel != AUTOMATION_CHANGED_EVENT {
            return Vec::new();
        }
        let automation = payload.get("automation_id").and_then(Value::as_i64);
        let run = payload
            .get("run_id")
            .and_then(Value::as_i64)
            .map(|r| r as i32);
        match (payload.get("kind").and_then(Value::as_str), automation, run) {
            (Some("run_started"), Some(a), Some(r)) => {
                self.runs.push(r);
                self.event(
                    "PreToolUse",
                    json!({
                        "tool_name": "Automatización",
                        "tool_input": { "query": format!("#{a} en curso") },
                    }),
                )
            }
            (Some("run_settled"), Some(a), Some(r)) => {
                let before = self.runs.len();
                self.runs.retain(|x| *x != r);
                if self.runs.len() == before {
                    return Vec::new();
                }
                let status = payload
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("terminada");
                let name = if status == "failed" {
                    "PostToolUseFailure"
                } else {
                    "PostToolUse"
                };
                let out = self.event(name, json!({ "tool_name": "Automatización" }));
                self.settle(out, &format!("Automatización #{a}: {status}"))
            }
            _ => Vec::new(),
        }
    }
}

fn base(name: &str) -> Value {
    json!({
        "hook_event_name": name,
        "session_id": SESSION_ID,
        "coucou_agent": AGENT,
    })
}

fn preview(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= PREVIEW_CHARS {
        return one_line;
    }
    let cut: String = one_line.chars().take(PREVIEW_CHARS - 1).collect();
    format!("{cut}…")
}

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("coucou.sock"))
}

#[cfg(unix)]
async fn send(path: &std::path::Path, event: &Value) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut stream = tokio::net::UnixStream::connect(path).await?;
    let mut line = event.to_string();
    line.push('\n');
    stream.write_all(line.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(not(unix))]
async fn send(_path: &std::path::Path, _event: &Value) -> std::io::Result<()> {
    Ok(())
}

/// Long-lived task: forwards Phantom's orchestration events to Coucou.
/// Disabled with `PHANTOM_COUCOU=0`.
pub fn coucou_bridge_task(
    bus: Arc<InternalEventBus>,
    broadcaster: Arc<WebEventBroadcaster>,
) -> impl std::future::Future<Output = ()> + Send + 'static {
    let mut acp_rx = bus.subscribe();
    let mut web_rx = broadcaster.subscribe();
    async move {
        if std::env::var("PHANTOM_COUCOU").as_deref() == Ok("0") {
            return;
        }
        let Some(path) = socket_path() else {
            return;
        };
        let mut state = BridgeState::default();
        loop {
            let events = tokio::select! {
                ev = acp_rx.recv() => match ev {
                    Ok(env) => state.on_acp(&env.connection_id, &env.payload),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Vec::new(),
                    Err(_) => return,
                },
                ev = web_rx.recv() => match ev {
                    Ok(web) => state.on_web(&web.channel, &web.payload),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => Vec::new(),
                    Err(_) => return,
                },
            };
            for event in events {
                if send(&path, &event).await.is_err() {
                    // Coucou is not running: resend SessionStart once it is back.
                    state.session_started = false;
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::agent::AgentType;

    fn names(v: &[Value]) -> Vec<&str> {
        v.iter()
            .map(|e| e["hook_event_name"].as_str().unwrap())
            .collect()
    }

    fn started(child: &str) -> AcpEvent {
        AcpEvent::DelegationStarted {
            parent_connection_id: "p".into(),
            parent_tool_use_id: "t".into(),
            child_connection_id: child.into(),
            child_conversation_id: 1,
            agent_type: AgentType::OpenCode,
            task_preview: "explore   the\nrepo".into(),
            task_id: "task".into(),
        }
    }

    fn completed(child: &str) -> AcpEvent {
        AcpEvent::DelegationCompleted {
            parent_connection_id: "p".into(),
            parent_tool_use_id: "t".into(),
            child_connection_id: child.into(),
            child_conversation_id: 1,
            agent_type: AgentType::OpenCode,
            result: DelegationResultSummary::Ok {
                duration_ms: 5,
                text_preview: Some("listo".into()),
            },
        }
    }

    #[test]
    fn delegations_open_the_pill_and_settle_only_when_all_finish() {
        let mut s = BridgeState::default();
        let first = s.on_acp("p", &started("c1"));
        assert_eq!(names(&first), ["SessionStart", "PreToolUse"]);
        assert_eq!(first[1]["coucou_agent"], AGENT);
        assert_eq!(first[1]["tool_name"], "Delegar → OpenCode");
        assert_eq!(first[1]["tool_input"]["query"], "explore the repo");
        assert_eq!(names(&s.on_acp("p", &started("c2"))), ["PreToolUse"]);
        assert_eq!(names(&s.on_acp("p", &completed("c1"))), ["PostToolUse"]);
        let last = s.on_acp("p", &completed("c2"));
        assert_eq!(names(&last), ["PostToolUse", "Stop"]);
        assert_eq!(last[1]["last_assistant_message"], "OpenCode: listo");
    }

    #[test]
    fn an_unknown_completion_is_ignored() {
        let mut s = BridgeState::default();
        assert!(s.on_acp("p", &completed("ghost")).is_empty());
    }

    #[test]
    fn a_pending_permission_asks_a_question_and_its_answer_settles() {
        let mut s = BridgeState::default();
        let ask = s.on_acp(
            "conn",
            &AcpEvent::PermissionRequest {
                request_id: "r1".into(),
                tool_call: json!({ "title": "rm -rf build" }),
                options: Vec::new(),
                queued: 0,
            },
        );
        assert_eq!(names(&ask), ["SessionStart", "Notification"]);
        assert!(ask[1]["message"].as_str().unwrap().ends_with("rm -rf build?"));
        let done = s.on_acp(
            "conn",
            &AcpEvent::PermissionResolved {
                request_id: "r1".into(),
            },
        );
        assert_eq!(names(&done), ["PostToolUse", "Stop"]);
    }

    #[test]
    fn automation_runs_map_to_tool_steps() {
        let mut s = BridgeState::default();
        let start = s.on_web(
            AUTOMATION_CHANGED_EVENT,
            &json!({ "kind": "run_started", "automation_id": 1, "run_id": 7 }),
        );
        assert_eq!(names(&start), ["SessionStart", "PreToolUse"]);
        let end = s.on_web(
            AUTOMATION_CHANGED_EVENT,
            &json!({ "kind": "run_settled", "automation_id": 1, "run_id": 7, "status": "failed" }),
        );
        assert_eq!(names(&end), ["PostToolUseFailure", "Stop"]);
        assert_eq!(end[1]["last_assistant_message"], "Automatización #1: failed");
        assert!(s
            .on_web("folder://changed", &json!({ "kind": "run_started" }))
            .is_empty());
    }

    #[test]
    fn preview_is_one_line_and_bounded() {
        let long = "a ".repeat(100);
        let p = preview(&long);
        assert!(p.chars().count() <= PREVIEW_CHARS);
        assert!(p.ends_with('…'));
    }
}
