//! Wire types for the model scorecard — measured, per-model quality and speed
//! numbers folded from `token_usage_turn`, joined against the live model
//! catalog (`acp::model_catalog`) and the bundled models.dev spec snapshot
//! (`acp::opencode_catalog`).
//!
//! See `commands::model_scorecard` for how every field is computed. Field
//! names here are load-bearing wire contract — the frontend is built against
//! this exact snake_case shape — so nothing is renamed and nothing is skipped
//! when `None`: every field always appears in the JSON, as either a value or
//! `null`.

use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct ModelScorecard {
    pub generated_at: DateTime<Utc>,
    pub models: Vec<ModelScorecardEntry>,
    pub best_for: Vec<BestForEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelScorecardEntry {
    pub agent_type: String,
    pub model: String,
    pub label: Option<String>,
    /// From the task-2 live catalog. `None` when codeg has never seen a
    /// `SessionConfigOptions` report for this `agent_type` at all — as
    /// opposed to `Some(false)`, which means the catalog IS known and this
    /// model is simply not (or no longer) in it.
    pub available: Option<bool>,
    pub conversations: u64,
    pub turns: u64,
    pub avg_turn_ms: Option<f64>,
    pub p50_turn_ms: Option<f64>,
    pub output_tokens_per_s: Option<f64>,
    pub output_tokens_per_turn: Option<f64>,
    pub cache_hit_pct: Option<f64>,
    pub tool_calls: u64,
    pub tool_error_pct: Option<f64>,
    pub category_calls: CategoryCounts,
    pub category_error_pct: CategoryErrorPct,
    pub last_used_at: Option<DateTime<Utc>>,
    pub spec: Option<ModelSpec>,
    /// Categories (`best_for[].category` values) this exact `(agent_type,
    /// model)` pair won. Empty for every model that isn't a category's best.
    pub strengths: Vec<String>,
    /// `true` when `turns < 20` — not a judgment on the model, a caveat on the
    /// number: this row's averages are one or two long sessions, not a trend.
    pub low_sample: bool,
    /// `true` when `acp::model_limits` currently has this `(agent_type,
    /// model)` marked as out of quota (an `account`-scoped hit covers every
    /// model of that agent; a `model`-scoped one covers only this row).
    /// Already-expired hits are dropped before this is computed, so `true`
    /// here always means "still in the lockout window". A limited model is
    /// excluded from every `best_for` ranking regardless of its measured
    /// numbers — see `commands::model_scorecard::is_ranking_eligible`.
    pub limited: bool,
    /// RFC 3339 timestamp the limit is expected to clear, when known.
    /// `None` when `limited` is `false`, or when it's `true` but codeg never
    /// parsed a reset time out of the failure message (still lazily expired
    /// after a default window — see `acp::model_limits`).
    pub limit_resets_at: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CategoryCounts {
    pub edit: u64,
    pub read: u64,
    pub shell: u64,
    pub web: u64,
    pub agent: u64,
}

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CategoryErrorPct {
    pub edit: Option<f64>,
    pub read: Option<f64>,
    pub shell: Option<f64>,
    pub web: Option<f64>,
    pub agent: Option<f64>,
}

/// From the bundled models.dev snapshot (`resources/opencode/models-dev.json`),
/// matched by id — see `commands::model_scorecard::spec_for`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ModelSpec {
    pub context: Option<u64>,
    pub reasoning: bool,
    pub tool_call: bool,
    pub cost_in: f64,
    pub cost_out: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BestForEntry {
    pub category: String,
    pub agent_type: String,
    pub model: String,
    pub metric: String,
    pub value: f64,
    pub sample: u64,
    pub runner_up: Option<RunnerUp>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunnerUp {
    pub agent_type: String,
    pub model: String,
    pub value: f64,
}
