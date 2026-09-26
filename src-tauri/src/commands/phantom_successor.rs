//! Model quota failover, selection half: given an agent (and optionally the
//! model that just hit a limit, and the conversation that was running),
//! answer "what should Phantom hand this conversation off to?" — using only
//! REAL measured data from `commands::model_scorecard`, never a static
//! preference table.
//!
//! [`phantom_successor_core`] is the pure-ish async shell (one scorecard
//! read, one optional per-conversation tool-category read, then a pure fold)
//! that both the HTTP handler (`web::handlers::phantom`) and the desktop
//! command wrap.

use std::cmp::Ordering;

use sea_orm::DatabaseConnection;

use crate::acp::model_limits::LimitScope;
use crate::app_error::AppCommandError;
use crate::commands::model_scorecard::{model_scorecard_core, wilson_upper_pct};
use crate::commands::token_usage::MAX_SCANNED_FACTS;
use crate::db::service::token_usage_service::{self as usage_service, FactQuery};
use crate::models::agent::AgentType;
use crate::models::model_scorecard::ModelScorecardEntry;
use crate::models::phantom::{
    PhantomLimitedInfo, PhantomSuccessorCandidate, PhantomSuccessorMetrics,
    PhantomSuccessorResponse,
};

/// A candidate must have at least this many tool calls to rank in the
/// "measured" tier — below it, it still ranks (nothing here excludes a
/// low-sample model, it just ranks after every measured one; a brand-new
/// agent must remain choosable). Mirrors `model_scorecard::LOW_SAMPLE_THRESHOLD`.
const MEASURED_TIER_THRESHOLD: u64 = 20;

pub async fn phantom_successor_core(
    conn: &DatabaseConnection,
    agent_type: AgentType,
    failing_model: Option<&str>,
    conversation_id: Option<i32>,
) -> Result<PhantomSuccessorResponse, AppCommandError> {
    let agent_wire = agent_type.as_wire().into_owned();

    let limits = crate::acp::model_limits::load_all(conn).await;
    let limited_entry = limits.get(&agent_wire);
    let limited = limited_entry.map(|l| PhantomLimitedInfo {
        agent_type: agent_wire.clone(),
        scope: l.scope,
        model: l.model.clone(),
        message: l.message.clone(),
        hit_at: l.hit_at,
        resets_hint: l.resets_hint.clone(),
        resets_at: l.resets_at,
    });
    let account_wide = limited_entry.is_some_and(|l| l.scope == LimitScope::Account);

    let card = model_scorecard_core(conn).await?;
    let dominant_category = match conversation_id {
        Some(cid) => dominant_tool_category(conn, cid).await,
        None => None,
    };

    let candidates = select_candidates(&card.models, &agent_wire, failing_model, account_wide);
    let ranked = rank_successors(&candidates, dominant_category.as_deref());
    let successor = ranked
        .first()
        .map(|e| to_candidate(e, dominant_category.as_deref()));
    let runner_up = ranked
        .get(1)
        .map(|e| to_candidate(e, dominant_category.as_deref()));

    Ok(PhantomSuccessorResponse {
        limited,
        successor,
        runner_up,
    })
}

/// Total tool-call counts, per category, for one conversation — read straight
/// from `token_usage_turn` via the same `fetch_facts` the scorecard itself
/// uses (there is no `conversation_id` filter on `FactQuery`, so this filters
/// in memory; a single conversation's row count is never large enough for
/// that to matter). `None` when the conversation has no recorded tool calls
/// in any of the four tracked categories, or the query itself fails.
async fn dominant_tool_category(conn: &DatabaseConnection, conversation_id: i32) -> Option<String> {
    let rows = usage_service::fetch_facts(conn, &FactQuery::default(), MAX_SCANNED_FACTS)
        .await
        .ok()?;
    let (mut edit, mut read, mut shell, mut web) = (0i64, 0i64, 0i64, 0i64);
    for row in rows.iter().filter(|r| r.conversation_id == conversation_id) {
        edit += row.edit_calls.max(0);
        read += row.read_calls.max(0);
        shell += row.shell_calls.max(0);
        web += row.web_calls.max(0);
    }
    [
        ("edit", edit),
        ("read", read),
        ("shell", shell),
        ("web", web),
    ]
    .into_iter()
    .filter(|(_, n)| *n > 0)
    .max_by_key(|(_, n)| *n)
    .map(|(name, _)| name.to_string())
}

/// The eligible candidate pool, pure and DB-free — every scorecard rule that
/// decides IN/OUT before ranking even starts:
/// - a real, non-empty, non-`"default"` model id
/// - `available != Some(false)` (unknown-catalog `None` stays eligible; only
///   an affirmative "not offered" excludes)
/// - not already `limited` (the scorecard side already merged
///   `acp::model_limits` in)
/// - not the exact model that just failed, on the same agent
/// - when `account_wide` is set (the failing agent's limit is account-scoped,
///   so EVERY model of that agent is out), no candidate from that agent at
///   all — there is no "other model, same agent" fallback for an
///   account-wide hit
fn select_candidates<'a>(
    models: &'a [ModelScorecardEntry],
    agent_wire: &str,
    failing_model: Option<&str>,
    account_wide: bool,
) -> Vec<&'a ModelScorecardEntry> {
    let failing_model = failing_model.map(str::trim).filter(|s| !s.is_empty());
    models
        .iter()
        .filter(|m| !m.model.is_empty() && m.model != "default")
        .filter(|m| m.available != Some(false))
        .filter(|m| !m.limited)
        .filter(|m| {
            !(m.agent_type == agent_wire
                && failing_model.is_some_and(|f| f.eq_ignore_ascii_case(&m.model)))
        })
        .filter(|m| !(account_wide && m.agent_type == agent_wire))
        .collect()
}

/// `(calls, error_pct)` for one of the four tracked tool categories on a
/// scorecard entry. `category` is expected to be one of `"edit"`/`"read"`/
/// `"shell"`/`"web"` (as [`dominant_tool_category`] produces); anything else
/// yields `(0, None)`, which callers treat as "no category signal".
fn category_calls_and_err(entry: &ModelScorecardEntry, category: &str) -> (u64, Option<f64>) {
    match category {
        "edit" => (entry.category_calls.edit, entry.category_error_pct.edit),
        "read" => (entry.category_calls.read, entry.category_error_pct.read),
        "shell" => (entry.category_calls.shell, entry.category_error_pct.shell),
        "web" => (entry.category_calls.web, entry.category_error_pct.web),
        _ => (0, None),
    }
}

/// The value ranking sorts on: the 95% Wilson upper bound of an error rate,
/// same shape as `model_scorecard::compute_best_for` — a small sample has to
/// earn its lead over a larger one with a slightly worse raw rate. Prefers
/// the dominant category's own error rate (when the entry has any calls in
/// it) over the overall tool error rate, so a conversation that was mostly
/// editing routes toward whichever candidate is actually good at edits.
fn rank_key(entry: &ModelScorecardEntry, dominant_category: Option<&str>) -> f64 {
    if let Some(cat) = dominant_category {
        let (calls, err_pct) = category_calls_and_err(entry, cat);
        if calls > 0 {
            return wilson_upper_pct(err_pct.unwrap_or(0.0), calls);
        }
    }
    wilson_upper_pct(entry.tool_error_pct.unwrap_or(0.0), entry.tool_calls)
}

/// Rank candidates best-first: every entry with `tool_calls >=
/// MEASURED_TIER_THRESHOLD` sorts ahead of every entry below it (a low-sample
/// model can still win within its own tier, but never jumps ahead of a
/// measured one purely on a lucky small sample), then by [`rank_key`]
/// ascending (lower error bound wins), tie-broken by higher throughput, then
/// by `(agent_type, model)` so the result is fully deterministic.
fn rank_successors<'a>(
    candidates: &[&'a ModelScorecardEntry],
    dominant_category: Option<&str>,
) -> Vec<&'a ModelScorecardEntry> {
    let (mut measured, mut low_sample): (Vec<&ModelScorecardEntry>, Vec<&ModelScorecardEntry>) =
        candidates
            .iter()
            .copied()
            .partition(|e| e.tool_calls >= MEASURED_TIER_THRESHOLD);
    let cmp = |a: &&ModelScorecardEntry, b: &&ModelScorecardEntry| -> Ordering {
        rank_key(a, dominant_category)
            .partial_cmp(&rank_key(b, dominant_category))
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.output_tokens_per_s
                    .unwrap_or(0.0)
                    .partial_cmp(&a.output_tokens_per_s.unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.agent_type.cmp(&b.agent_type))
            .then_with(|| a.model.cmp(&b.model))
    };
    measured.sort_by(cmp);
    low_sample.sort_by(cmp);
    measured.into_iter().chain(low_sample).collect()
}

/// Format a percentage the way a Spanish sentence expects it: one decimal,
/// comma instead of a point (`0.2` -> `"0,2"`).
fn es_decimal(v: f64) -> String {
    format!("{v:.1}").replace('.', ",")
}

/// Spanish plural noun for one tool category, used in [`build_reason`].
fn category_noun_es(category: &str) -> &'static str {
    match category {
        "edit" => "ediciones",
        "read" => "lecturas",
        "shell" => "comandos",
        "web" => "búsquedas web",
        _ => "llamadas a herramientas",
    }
}

/// Short Spanish sentence justifying why `entry` was picked, with the actual
/// measured numbers — e.g. "Mejor disponible: 0,2 % de errores en 585
/// ediciones, 55 tok/s". Prefers the dominant-category numbers (matching what
/// [`rank_key`] actually ranked on) when the entry has calls in that
/// category; otherwise falls back to the overall tool stats.
fn build_reason(entry: &ModelScorecardEntry, dominant_category: Option<&str>) -> String {
    let throughput = entry
        .output_tokens_per_s
        .map(|v| format!("{v:.0} tok/s"))
        .unwrap_or_else(|| "velocidad aún sin medir".to_string());

    if let Some(cat) = dominant_category {
        let (calls, err_pct) = category_calls_and_err(entry, cat);
        if let Some(err) = err_pct.filter(|_| calls > 0) {
            return format!(
                "Mejor disponible: {} % de errores en {} {}, {throughput}",
                es_decimal(err),
                calls,
                category_noun_es(cat),
            );
        }
    }

    match entry.tool_error_pct {
        Some(err) => format!(
            "Mejor disponible: {} % de errores en {} llamadas a herramientas, {throughput}",
            es_decimal(err),
            entry.tool_calls,
        ),
        None => format!("Mejor disponible: sin errores medidos aún, {throughput}"),
    }
}

fn to_candidate(
    entry: &ModelScorecardEntry,
    dominant_category: Option<&str>,
) -> PhantomSuccessorCandidate {
    PhantomSuccessorCandidate {
        agent_type: entry.agent_type.clone(),
        model: entry.model.clone(),
        label: entry.label.clone(),
        reason: build_reason(entry, dominant_category),
        metrics: PhantomSuccessorMetrics {
            turns: entry.turns,
            tool_error_pct: entry.tool_error_pct,
            output_tokens_per_s: entry.output_tokens_per_s,
            context: entry.spec.as_ref().and_then(|s| s.context),
        },
    }
}

// ─── Desktop command ─────────────────────────────────────────────────────

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn phantom_successor(
    db: tauri::State<'_, crate::db::AppDatabase>,
    agent_type: AgentType,
    model: Option<String>,
    conversation_id: Option<i32>,
) -> Result<PhantomSuccessorResponse, AppCommandError> {
    phantom_successor_core(&db.conn, agent_type, model.as_deref(), conversation_id).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::model_scorecard::build_scorecard;
    use crate::db::service::token_usage_service::UsageFactRow;
    use chrono::{DateTime, Utc};
    use std::collections::HashMap;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn row(agent: &str, model: &str, conv: i32) -> UsageFactRow {
        UsageFactRow {
            conversation_id: conv,
            folder_id: 1,
            agent_type: agent.to_string(),
            model: Some(model.to_string()),
            occurred_at: ts("2026-09-24T00:00:00Z"),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 150,
            duration_ms: 1000,
            tool_calls: 1,
            tool_errors: 0,
            edit_calls: 0,
            edit_errors: 0,
            read_calls: 0,
            read_errors: 0,
            shell_calls: 0,
            shell_errors: 0,
            web_calls: 0,
            web_errors: 0,
            agent_calls: 0,
            agent_errors: 0,
        }
    }

    fn rows_for(agent: &str, model: &str, n: i32, errors: i32) -> Vec<UsageFactRow> {
        (0..n)
            .map(|i| {
                let mut r = row(agent, model, i);
                r.tool_errors = if i < errors { 1 } else { 0 };
                r.duration_ms = 1000;
                r.output_tokens = 100;
                r
            })
            .collect()
    }

    // ─── rank_successors ────────────────────────────────────────────────

    #[test]
    fn measured_candidates_rank_ahead_of_low_sample_ones() {
        let now = ts("2026-09-24T00:00:00Z");
        let mut rows = rows_for("open_code", "measured-model", 25, 1); // 4% errors, measured
        rows.extend(rows_for("open_code", "fresh-model", 3, 0)); // 0% errors, low-sample
        let card = build_scorecard(&rows, &HashMap::new(), &HashMap::new(), now);
        let candidates: Vec<&ModelScorecardEntry> = card.models.iter().collect();
        let ranked = rank_successors(&candidates, None);
        assert_eq!(
            ranked[0].model, "measured-model",
            "measured tier ranks first regardless of the low-sample model's perfect record"
        );
    }

    #[test]
    fn within_a_tier_lower_wilson_bound_wins() {
        let now = ts("2026-09-24T00:00:00Z");
        let mut rows = rows_for("open_code", "worse", 30, 6); // 20% errors
        rows.extend(rows_for("open_code", "better", 30, 3)); // 10% errors
        let card = build_scorecard(&rows, &HashMap::new(), &HashMap::new(), now);
        let candidates: Vec<&ModelScorecardEntry> = card.models.iter().collect();
        let ranked = rank_successors(&candidates, None);
        assert_eq!(ranked[0].model, "better");
    }

    #[test]
    fn dominant_category_prefers_the_category_specific_error_rate() {
        let now = ts("2026-09-24T00:00:00Z");
        // model-a: bad at edits (30%) but good overall (due to other calls).
        let mut a = rows_for("open_code", "model-a", 25, 0);
        for (i, r) in a.iter_mut().enumerate() {
            r.edit_calls = 1;
            r.edit_errors = if i < 8 { 1 } else { 0 }; // ~32% edit errors
        }
        // model-b: good at edits (0%).
        let mut b = rows_for("open_code", "model-b", 25, 0);
        for r in b.iter_mut() {
            r.edit_calls = 1;
        }
        let mut rows = a;
        rows.extend(b);
        let card = build_scorecard(&rows, &HashMap::new(), &HashMap::new(), now);
        let candidates: Vec<&ModelScorecardEntry> = card.models.iter().collect();
        let ranked = rank_successors(&candidates, Some("edit"));
        assert_eq!(ranked[0].model, "model-b");
    }

    // ─── select_candidates: scope + exclusion rules ──────────────────────

    #[allow(clippy::too_many_arguments)]
    fn entry(
        agent_type: &str,
        model: &str,
        available: Option<bool>,
        limited: bool,
    ) -> ModelScorecardEntry {
        ModelScorecardEntry {
            agent_type: agent_type.to_string(),
            model: model.to_string(),
            label: None,
            available,
            conversations: 0,
            turns: 0,
            avg_turn_ms: None,
            p50_turn_ms: None,
            output_tokens_per_s: None,
            output_tokens_per_turn: None,
            cache_hit_pct: None,
            tool_calls: 0,
            tool_error_pct: None,
            category_calls: crate::models::model_scorecard::CategoryCounts::default(),
            category_error_pct: crate::models::model_scorecard::CategoryErrorPct::default(),
            last_used_at: None,
            spec: None,
            strengths: Vec::new(),
            low_sample: true,
            limited,
            limit_resets_at: None,
        }
    }

    #[test]
    fn model_scope_excludes_only_the_exact_failing_model() {
        let models = vec![
            entry("claude_code", "claude-opus-5", Some(true), false),
            entry("claude_code", "claude-sonnet-5", Some(true), false),
        ];
        let candidates = select_candidates(&models, "claude_code", Some("claude-opus-5"), false);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].model, "claude-sonnet-5");
    }

    #[test]
    fn account_scope_excludes_every_model_of_the_failing_agent() {
        let models = vec![
            entry("claude_code", "claude-opus-5", Some(true), false),
            entry("claude_code", "claude-sonnet-5", Some(true), false),
            entry("open_code", "nemotron", Some(true), false),
        ];
        // account_wide=true with no single failing model — every claude_code
        // row must go, not just the one that happened to be active.
        let candidates = select_candidates(&models, "claude_code", None, true);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].agent_type, "open_code");
    }

    #[test]
    fn already_limited_models_are_excluded_regardless_of_which_agent_is_failing() {
        let models = vec![
            entry("claude_code", "claude-opus-5", Some(true), true),
            entry("open_code", "nemotron", Some(true), false),
        ];
        let candidates = select_candidates(&models, "codex", None, false);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].model, "nemotron");
    }

    #[test]
    fn unavailable_models_are_excluded_but_unknown_availability_is_kept() {
        let models = vec![
            entry("open_code", "retired", Some(false), false),
            entry("open_code", "current", Some(true), false),
            entry("hermes", "gpt-4o", None, false),
        ];
        let candidates = select_candidates(&models, "claude_code", None, false);
        let names: Vec<&str> = candidates.iter().map(|m| m.model.as_str()).collect();
        assert!(!names.contains(&"retired"));
        assert!(names.contains(&"current"));
        assert!(names.contains(&"gpt-4o"));
    }

    #[test]
    fn empty_and_default_model_ids_never_appear_as_candidates() {
        let models = vec![
            entry("claude_code", "", Some(true), false),
            entry("claude_code", "default", Some(true), false),
            entry("claude_code", "claude-sonnet-5", Some(true), false),
        ];
        let candidates = select_candidates(&models, "codex", None, false);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].model, "claude-sonnet-5");
    }

    // ─── phantom_successor_core: end-to-end smoke test ───────────────────

    #[tokio::test]
    async fn phantom_successor_core_on_an_empty_database_returns_no_successor() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let resp = phantom_successor_core(&db.conn, AgentType::ClaudeCode, None, None)
            .await
            .expect("core");
        assert!(resp.limited.is_none());
        assert!(resp.successor.is_none());
        assert!(resp.runner_up.is_none());
    }

    #[test]
    fn category_noun_covers_every_tracked_category() {
        for cat in ["edit", "read", "shell", "web", "agent"] {
            assert!(!category_noun_es(cat).is_empty());
        }
    }

    #[test]
    fn es_decimal_uses_a_comma() {
        assert_eq!(es_decimal(0.2), "0,2");
    }
}
