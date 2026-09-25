//! Live model catalog: the last model list each agent actually advertised.
//!
//! Every ACP connection reports its available models the same way it reports
//! any other selectable setting — as a `category: "model"` entry inside its
//! `SessionConfigOptions` report (see `acp::connection::MODEL_CONFIG_OPTION_ID`
//! and `emit_session_config_options_values` / `emit_session_config_options_info`,
//! the two producers of that event). [`record_seen`] is called from the single
//! choke point every one of those producers already funnels through —
//! [`crate::web::event_bridge::emit_with_state_gated`], right after the event
//! is applied to `SessionState` — so this module needs no changes to the
//! connection-handling code itself, and sees a model list regardless of which
//! agent, which producer, or whether the connection is a real user session or
//! a delegation-settings probe.
//!
//! The write is deliberately decoupled from that choke point's own critical
//! section: [`record_seen`] does a cheap, synchronous extraction (no I/O) and
//! then `tokio::spawn`s the actual database write. A DB hiccup here must never
//! stall an ACP event on the hot path that every connection shares, so a
//! failure is logged and dropped rather than retried or surfaced anywhere the
//! connection would notice.
//!
//! Storage: one `app_metadata` row per agent, key `phantom.model_catalog.<wire
//! agent type>`, value a JSON [`StoredCatalog`]. Idempotent by construction —
//! each report simply replaces the previous one (`app_metadata_service::
//! upsert_value`), so a duplicate or out-of-order report costs nothing beyond
//! the write itself.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter};
use serde::{Deserialize, Serialize};

use crate::acp::types::{SessionConfigKindInfo, SessionConfigOptionInfo};
use crate::db::entities::app_metadata;
use crate::db::service::app_metadata_service;
use crate::models::AgentType;

/// Prefix of every `app_metadata` key this module writes / reads. The suffix
/// is `AgentType::as_wire()` — the same string already stored in
/// `conversation.agent_type`.
pub const KEY_PREFIX: &str = "phantom.model_catalog.";

/// The conventional id/category of a model selector. Mirrors the private
/// `MODEL_CONFIG_OPTION_ID` in `acp::connection` — duplicated rather than made
/// `pub(crate)` there, since that module is dense enough already and this is
/// the one place outside it that needs the string.
const MODEL_CONFIG_OPTION_ID: &str = "model";

/// One model an agent advertised.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalogEntry {
    pub id: String,
    /// The display name the agent gave it, when that differs from the id
    /// (identical or blank names are not worth carrying twice).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// What gets persisted for one agent: its last-seen model list and when it was
/// seen.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredCatalog {
    pub models: Vec<ModelCatalogEntry>,
    pub seen_at: DateTime<Utc>,
}

/// Set once at startup by [`init`]. `record_seen` is a no-op (besides a debug
/// log) until this is set — the earliest a connection can report config
/// options is well after both binaries finish constructing their database, so
/// in practice there is no window where a real report is dropped for lacking
/// this.
static DB_CONN: OnceLock<DatabaseConnection> = OnceLock::new();

/// Wire up the database connection this module writes through. Call once at
/// startup, after the app's `DatabaseConnection` exists — see
/// `codeg_server.rs` and `lib.rs` (desktop `setup`) for the two call sites.
/// A second call is a silent no-op (matches every other startup wiring
/// function in this codebase — the first writer wins).
pub fn init(conn: DatabaseConnection) {
    let _ = DB_CONN.set(conn);
}

/// Refresh the measured routing guide on disk and return its path, for
/// `codeg-mcp --routing-guide-file`. `None` when the database is not wired
/// yet, there is no data to report, or the write fails — the companion then
/// simply runs without a guide. Written via a temp file + rename so a
/// companion reading it concurrently never sees a torn file.
pub async fn refresh_routing_guide_file() -> Option<std::path::PathBuf> {
    let conn = DB_CONN.get()?;
    let text = crate::commands::model_scorecard::routing_guide_text(conn).await;
    if text.trim().is_empty() {
        return None;
    }
    let dir = crate::paths::codeg_home_dir().join("phantom");
    let path = dir.join("routing-guide.txt");
    let write = || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!("routing-guide.{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&tmp, text.as_bytes())?;
        std::fs::rename(&tmp, &path)
    };
    match write() {
        Ok(()) => Some(path),
        Err(e) => {
            tracing::warn!("[model_catalog] could not write routing guide: {e}");
            None
        }
    }
}

/// Extract the model option's advertised list from a `SessionConfigOptions`
/// report, if it has one. `None` when the report carries no `category:
/// "model"` (or `id: "model"`) selector, or when that selector's option list
/// is empty — both are "nothing to record", not "record an empty catalog"
/// (an agent still mid-handshake can legitimately report other selectors
/// before its model list is ready).
fn extract_model_entries(options: &[SessionConfigOptionInfo]) -> Option<Vec<ModelCatalogEntry>> {
    let model_option = options.iter().find(|o| {
        o.category.as_deref() == Some(MODEL_CONFIG_OPTION_ID) || o.id == MODEL_CONFIG_OPTION_ID
    })?;
    let SessionConfigKindInfo::Select(select) = &model_option.kind else {
        return None;
    };
    if select.options.is_empty() {
        return None;
    }
    Some(
        select
            .options
            .iter()
            .map(|o| ModelCatalogEntry {
                id: o.value.clone(),
                label: {
                    let name = o.name.trim();
                    if name.is_empty() || name == o.value {
                        None
                    } else {
                        Some(name.to_string())
                    }
                },
            })
            .collect(),
    )
}

/// Record the model list a `SessionConfigOptions` report carried for
/// `agent_type`, if it carries one. Cheap and synchronous up to this point;
/// the actual persistence is a spawned task, so this never blocks the caller.
///
/// Called from [`crate::web::event_bridge::emit_with_state_gated`] for every
/// `AcpEvent::SessionConfigOptions`, which fires on session establishment, on
/// a live `set_config_option` round-trip, and on an agent-initiated push —
/// i.e. on every occasion an agent tells codeg what models it currently
/// offers.
pub fn record_seen(agent_type: AgentType, options: &[SessionConfigOptionInfo]) {
    let Some(models) = extract_model_entries(options) else {
        return;
    };
    let Some(conn) = DB_CONN.get().cloned() else {
        tracing::debug!(
            agent_type = %agent_type.as_wire(),
            "model_catalog: database not wired up yet, dropping a catalog report"
        );
        return;
    };
    let agent_wire = agent_type.as_wire().into_owned();
    tokio::spawn(async move {
        if let Err(e) = persist(&conn, &agent_wire, models).await {
            tracing::warn!(
                agent_type = %agent_wire,
                error = %e,
                "model_catalog: failed to persist a catalog report"
            );
        }
    });
}

async fn persist(
    conn: &DatabaseConnection,
    agent_wire: &str,
    models: Vec<ModelCatalogEntry>,
) -> Result<(), crate::db::error::DbError> {
    let payload = StoredCatalog {
        models,
        seen_at: Utc::now(),
    };
    // A payload this small (a few dozen short strings at most) is not worth a
    // fallible-serialization dance; an empty object is a harmless worst case.
    let json = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    let key = format!("{KEY_PREFIX}{agent_wire}");
    app_metadata_service::upsert_value(conn, &key, &json).await
}

/// Read every agent's last-seen catalog, keyed by wire agent type
/// (`AgentType::as_wire()`). Used by the model scorecard to mark a model
/// `available` / not, and to list catalog-only models the user hasn't used
/// yet. A row that fails to parse (should not happen — this module is the
/// only writer) is skipped rather than failing the whole read.
pub async fn load_all(
    conn: &DatabaseConnection,
) -> std::collections::HashMap<String, StoredCatalog> {
    let rows = app_metadata::Entity::find()
        .filter(app_metadata::Column::Key.starts_with(KEY_PREFIX))
        .filter(app_metadata::Column::DeletedAt.is_null())
        .all(conn)
        .await
        .unwrap_or_default();
    rows.into_iter()
        .filter_map(|r| {
            let agent_wire = r.key.strip_prefix(KEY_PREFIX)?.to_string();
            let parsed = serde_json::from_str::<StoredCatalog>(&r.value).ok()?;
            Some((agent_wire, parsed))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::types::{
        SessionConfigBooleanInfo, SessionConfigSelectInfo, SessionConfigSelectOptionInfo,
    };

    fn select_option(
        id: &str,
        category: &str,
        options: Vec<(&str, &str)>,
    ) -> SessionConfigOptionInfo {
        SessionConfigOptionInfo {
            id: id.to_string(),
            name: "Model".to_string(),
            description: None,
            category: Some(category.to_string()),
            kind: SessionConfigKindInfo::Select(SessionConfigSelectInfo {
                current_value: options
                    .first()
                    .map(|(v, _)| v.to_string())
                    .unwrap_or_default(),
                options: options
                    .into_iter()
                    .map(|(value, name)| SessionConfigSelectOptionInfo {
                        value: value.to_string(),
                        name: name.to_string(),
                        description: None,
                    })
                    .collect(),
                groups: Vec::new(),
            }),
            recommended_value: None,
        }
    }

    #[test]
    fn extracts_the_model_selector_by_category() {
        let opts = vec![select_option(
            "model",
            "model",
            vec![("claude-opus-5", "Opus 5"), ("claude-sonnet-5", "Sonnet 5")],
        )];
        let entries = extract_model_entries(&opts).expect("some entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, "claude-opus-5");
        assert_eq!(entries[0].label.as_deref(), Some("Opus 5"));
    }

    #[test]
    fn falls_back_to_the_conventional_id_when_category_is_absent() {
        let mut opt = select_option("model", "model", vec![("gpt-5", "GPT-5")]);
        opt.category = None;
        let entries = extract_model_entries(&[opt]).expect("some entries");
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn a_name_identical_or_blank_collapses_to_no_label() {
        let opts = vec![select_option(
            "model",
            "model",
            vec![("grok-4", "grok-4"), ("grok-4.5", "")],
        )];
        let entries = extract_model_entries(&opts).expect("some entries");
        assert_eq!(entries[0].label, None, "identical name/id is not a label");
        assert_eq!(entries[1].label, None, "blank name is not a label");
    }

    #[test]
    fn no_model_selector_yields_none() {
        let opts = vec![SessionConfigOptionInfo {
            id: "auto_approve".to_string(),
            name: "Auto-approve".to_string(),
            description: None,
            category: Some("other".to_string()),
            kind: SessionConfigKindInfo::Boolean(SessionConfigBooleanInfo {
                current_value: true,
            }),
            recommended_value: None,
        }];
        assert!(extract_model_entries(&opts).is_none());
        assert!(extract_model_entries(&[]).is_none());
    }

    #[test]
    fn an_empty_model_option_list_yields_none_not_an_empty_catalog() {
        let opts = vec![select_option("model", "model", vec![])];
        assert!(extract_model_entries(&opts).is_none());
    }

    #[tokio::test]
    async fn load_all_round_trips_what_persist_writes() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        persist(
            &db.conn,
            "claude_code",
            vec![ModelCatalogEntry {
                id: "claude-opus-5".to_string(),
                label: Some("Opus 5".to_string()),
            }],
        )
        .await
        .expect("persist");

        let all = load_all(&db.conn).await;
        let stored = all.get("claude_code").expect("stored catalog");
        assert_eq!(stored.models.len(), 1);
        assert_eq!(stored.models[0].id, "claude-opus-5");

        // A re-report replaces rather than accumulates.
        persist(
            &db.conn,
            "claude_code",
            vec![
                ModelCatalogEntry {
                    id: "claude-opus-5".to_string(),
                    label: None,
                },
                ModelCatalogEntry {
                    id: "claude-sonnet-5".to_string(),
                    label: None,
                },
            ],
        )
        .await
        .expect("persist again");
        let all = load_all(&db.conn).await;
        assert_eq!(all.get("claude_code").expect("stored").models.len(), 2);
    }
}
