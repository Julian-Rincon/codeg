//! Wire types for Phantom's model-quota-failover endpoints
//! (`commands::phantom_successor`, `commands::phantom_handoff`). See those
//! modules for how each field is computed.
//!
//! Field names are the wire contract (snake_case, matching every other
//! `models::*` type in this codebase) — the frontend/Telegram wiring is built
//! against this exact shape.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::acp::model_limits::LimitScope;

/// The limit currently recorded for the agent the caller asked about, if any
/// — `acp::model_limits::StoredLimit` plus the `agent_type` it belongs to
/// (the stored map is already keyed by it, so the caller doesn't have to
/// echo it back in).
#[derive(Debug, Clone, Serialize)]
pub struct PhantomLimitedInfo {
    pub agent_type: String,
    pub scope: LimitScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub message: String,
    pub hit_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<DateTime<Utc>>,
}

/// The measured numbers behind one successor `reason` — a subset of
/// `ModelScorecardEntry`, only the fields the recommendation is justified by.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct PhantomSuccessorMetrics {
    pub turns: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_error_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_tokens_per_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
}

/// One candidate — always a real `(agent_type, model)` pair drawn from that
/// agent's own measured/catalog data, never a synthesized cross-agent
/// suggestion.
#[derive(Debug, Clone, Serialize)]
pub struct PhantomSuccessorCandidate {
    pub agent_type: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Short Spanish sentence with the numbers, e.g. "Mejor disponible: 0,2 %
    /// de errores en 585 ediciones, 55 tok/s".
    pub reason: String,
    pub metrics: PhantomSuccessorMetrics,
}

#[derive(Debug, Clone, Serialize)]
pub struct PhantomSuccessorResponse {
    pub limited: Option<PhantomLimitedInfo>,
    pub successor: Option<PhantomSuccessorCandidate>,
    pub runner_up: Option<PhantomSuccessorCandidate>,
}

/// Result of `commands::phantom_handoff::phantom_handoff_core` — the new
/// conversation the failover created.
#[derive(Debug, Clone, Serialize)]
pub struct PhantomHandoffResponse {
    pub conversation_id: i32,
    pub folder_id: i32,
    pub agent_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub connection_id: String,
}
