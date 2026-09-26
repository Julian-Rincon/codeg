//! Model quota failover, detection half: notice an agent connection reporting
//! that it is out of tokens/usage, and persist that fact so `model_scorecard`
//! can exclude it from `best_for` and `commands::phantom_successor` can route
//! around it.
//!
//! Mirrors `acp::model_catalog`'s wiring end to end: [`detect_limit`] is a
//! cheap, synchronous read called from the single choke point every ACP event
//! funnels through — [`crate::web::event_bridge::emit_with_state_gated`],
//! right after the event is applied to `SessionState` — and [`record_hit`]
//! hands the actual database write to a spawned task so a DB hiccup can never
//! stall the hot event path. Unlike the catalog (one `app_metadata` row per
//! agent), every agent's current limit lives in ONE row — `KEY` — as a JSON
//! map, because a limit hit is rare enough that the extra read-modify-write
//! this costs is a non-issue, and a single row means `model_scorecard` (which
//! wants every agent's state at once) never has to scan by key prefix.
//!
//! Detection is necessarily a heuristic: ACP carries no first-class "you are
//! out of quota" signal. codeg reads the JetBrains AIR `SessionFailure`
//! `category: "limit"` upsert when the adapter has one, and falls back to
//! pattern-matching the free-text `Error` message every adapter always has —
//! see [`detect_limit`] for the exact rules.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use chrono::{DateTime, TimeZone, Utc};
use regex::Regex;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use crate::acp::types::{AcpEvent, SessionFailureRecord};
use crate::db::error::DbError;
use crate::db::service::app_metadata_service;
use crate::models::AgentType;

/// The single `app_metadata` row this module reads and writes. Value is a
/// JSON object keyed by `AgentType::as_wire()`, one entry per agent that
/// currently has an outstanding limit.
pub const KEY: &str = "phantom.model_limits";

/// When no reset time could be parsed out of the failure text, an entry is
/// still lazily expired this long after it was recorded — a stuck "limited"
/// flag that nothing ever clears would silently blackhole an agent forever.
const DEFAULT_EXPIRY_HOURS: i64 = 5;

/// Whether a detected limit covers the whole agent account/session or just
/// the one model that was active when it hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope {
    /// The failure text names the account/plan/session as a whole (Claude's
    /// "session limit" / "usage limit" phrasing) — every model this agent
    /// offers is unusable until it clears.
    Account,
    /// The failure names a specific model or a provider-level rate limit
    /// (OpenRouter 429, "insufficient_quota", …) — only the model active on
    /// the connection at the time is affected.
    Model,
}

/// What [`detect_limit`] found in one event, before the caller has resolved
/// which model (if any) was active on the connection.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitHit {
    pub scope: LimitScope,
    /// The adapter's own wording, for display — `SessionFailureRecord::title`
    /// or the `Error` event's `message`, whichever produced the match.
    pub message: String,
    /// The raw substring that suggested a reset time (`"resets 5am"`,
    /// `"try again in 3h"`, …), kept verbatim for display even when
    /// [`Self::resets_at`] could not be computed from it.
    pub resets_hint: Option<String>,
    /// Best-effort absolute reset time, in UTC.
    pub resets_at: Option<DateTime<Utc>>,
}

/// What gets persisted for one agent inside the [`KEY`] JSON map.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredLimit {
    pub scope: LimitScope,
    /// The model this hit applies to. Always `None` for [`LimitScope::Account`]
    /// (the whole agent is out, not one model); may still be `None` for
    /// [`LimitScope::Model`] when the connection's active model could not be
    /// resolved at hit time (an agent with no model selector at all).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub model: Option<String>,
    pub message: String,
    pub hit_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resets_hint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub resets_at: Option<DateTime<Utc>>,
}

impl StoredLimit {
    /// The instant this entry should stop being treated as limited: its own
    /// `resets_at` when known, otherwise [`DEFAULT_EXPIRY_HOURS`] after
    /// `hit_at`.
    fn expires_at(&self) -> DateTime<Utc> {
        self.resets_at
            .unwrap_or_else(|| self.hit_at + chrono::Duration::hours(DEFAULT_EXPIRY_HOURS))
    }

    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now >= self.expires_at()
    }
}

type LimitsMap = BTreeMap<String, StoredLimit>;

/// Set once at startup by [`init`] — see `acp::model_catalog::init` for why a
/// second call is a silent no-op and why the startup ordering makes the
/// "dropped for lacking a database" window a non-issue in practice.
static DB_CONN: OnceLock<DatabaseConnection> = OnceLock::new();

pub fn init(conn: DatabaseConnection) {
    let _ = DB_CONN.set(conn);
}

// ─── Detection ──────────────────────────────────────────────────────────

/// Case-insensitive substrings that, anywhere in an `Error`/failure message,
/// mean "this is a quota/rate exhaustion, not an ordinary failure". Kept as
/// one flat list rather than a regex — these are adapter-authored free text
/// from at least three different vendors, so a substring match is both the
/// most robust option and the easiest to extend.
const QUOTA_PATTERNS: &[&str] = &[
    "session limit",
    "usage limit",
    "rate limit",
    "rate_limit",
    "quota",
    "insufficient_quota",
    "out of credits",
    "credit balance",
    "spend limit",
    "exceeded your",
    "too many requests",
    "429",
];

/// Narrower subset used to promote a `severity: "warning"` `SessionFailure`
/// to a real hit. A warning-level AIR record is usually a transient,
/// auto-recovering condition (see the type's own docs); only promote it when
/// the text itself says the quota is actually spent, not merely throttled —
/// "rate limit" / "too many requests" / "429" stay error-severity-only.
const EXHAUSTED_PATTERNS: &[&str] = &[
    "session limit",
    "usage limit",
    "quota",
    "insufficient_quota",
    "out of credits",
    "credit balance",
    "spend limit",
    "exceeded your",
];

/// Phrasing that means the WHOLE account/session is out, as opposed to one
/// model or provider key. Anything else defaults to [`LimitScope::Model`] —
/// see the module doc and the type's own docs for why that is the safer
/// default (a model-scoped false-Account would silently take every agent
/// model out of rotation).
const ACCOUNT_SCOPE_PATTERNS: &[&str] = &["session limit", "usage limit", "plan limit"];

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    let lower = haystack.to_lowercase();
    needles.iter().any(|n| lower.contains(n))
}

fn classify_scope(text: &str) -> LimitScope {
    if contains_any(text, ACCOUNT_SCOPE_PATTERNS) {
        LimitScope::Account
    } else {
        LimitScope::Model
    }
}

fn detect_from_failure(record: &SessionFailureRecord) -> Option<LimitHit> {
    if record.category != "limit" {
        return None;
    }
    let combined = format!(
        "{} {}",
        record.title,
        record.details.as_deref().unwrap_or_default()
    );
    if record.severity == "warning" && !contains_any(&combined, EXHAUSTED_PATTERNS) {
        return None;
    }
    let scope = classify_scope(&combined);
    let (resets_hint, resets_at) = parse_reset_hint(&combined);
    let message = if record.title.trim().is_empty() {
        combined.trim().to_string()
    } else {
        record.title.clone()
    };
    Some(LimitHit {
        scope,
        message,
        resets_hint,
        resets_at,
    })
}

fn detect_from_text(message: &str, details: Option<&str>) -> Option<LimitHit> {
    let combined = format!("{message} {}", details.unwrap_or_default());
    if !contains_any(&combined, QUOTA_PATTERNS) {
        return None;
    }
    let scope = classify_scope(&combined);
    let (resets_hint, resets_at) = parse_reset_hint(&combined);
    Some(LimitHit {
        scope,
        message: message.to_string(),
        resets_hint,
        resets_at,
    })
}

/// Inspect one ACP event for a quota/usage-limit signal. `agent_type` is not
/// read (detection is purely text-based) but is accepted — and logged when a
/// hit is found — so a caller never has to thread agent identity through a
/// second call just for observability.
///
/// Checks, in order: a JetBrains AIR `SessionFailure` whose `category` is
/// `"limit"` (any `severity: "error"`; `severity: "warning"` only when the
/// text itself says the quota is exhausted rather than merely throttled —
/// see [`EXHAUSTED_PATTERNS`]), then a plain `Error` event whose `message`/
/// `details` matches one of [`QUOTA_PATTERNS`]. Every other event variant —
/// including the non-terminal `TurnRetrying` — is not inspected: a retry
/// banner is by definition not a spent quota.
pub fn detect_limit(agent_type: AgentType, event: &AcpEvent) -> Option<LimitHit> {
    let hit = match event {
        AcpEvent::SessionFailure { record } => detect_from_failure(record),
        AcpEvent::Error {
            message, details, ..
        } => detect_from_text(message, details.as_deref()),
        _ => None,
    }?;
    tracing::info!(
        agent_type = %agent_type.as_wire(),
        scope = ?hit.scope,
        resets_hint = ?hit.resets_hint,
        "model_limits: detected a quota/usage-limit hit"
    );
    Some(hit)
}

// ─── Reset-time parsing ─────────────────────────────────────────────────

fn reset_clock_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)resets?\s*(?:at\s*)?(\d{1,2})(?::(\d{2}))?\s*(am|pm)(?:\s*\(([A-Za-z0-9_/+-]+)\))?",
        )
        .expect("reset-clock pattern is a valid regex")
    })
}

fn try_again_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)try again in\s*(\d+)\s*(hours?|hrs?|h|minutes?|mins?|m|seconds?|secs?|s)\b",
        )
        .expect("try-again pattern is a valid regex")
    })
}

fn retry_after_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)retry-after:?\s*(\d+)").expect("retry-after pattern is a valid regex")
    })
}

fn to_24h(hour12: u32, meridiem: &str) -> u32 {
    let h = hour12 % 12;
    if meridiem.eq_ignore_ascii_case("pm") {
        h + 12
    } else {
        h
    }
}

fn duration_for(n: i64, unit: &str) -> chrono::Duration {
    let unit = unit.to_lowercase();
    if unit.starts_with('h') {
        chrono::Duration::hours(n)
    } else if unit.starts_with('m') {
        chrono::Duration::minutes(n)
    } else {
        chrono::Duration::seconds(n)
    }
}

/// Next wall-clock occurrence of `hour:minute` in timezone `tz`, at or after
/// `now`. Generic over `chrono::TimeZone` so the same logic serves both a
/// parsed IANA name (`chrono_tz::Tz`) and the process's local timezone
/// (`chrono::Local`) — see [`compute_reset_at`]. `None` only for a
/// pathological (nonexistent/ambiguous with no earliest) local time, which a
/// clock-hour input never produces in practice.
fn next_occurrence_utc<Z: TimeZone>(tz: Z, hour: u32, minute: u32) -> Option<DateTime<Utc>>
where
    Z::Offset: Copy,
{
    let now_in_tz = Utc::now().with_timezone(&tz);
    let today = now_in_tz.date_naive();
    let naive = today.and_hms_opt(hour, minute, 0)?;
    let mut candidate = tz.from_local_datetime(&naive).earliest()?;
    if candidate <= now_in_tz {
        candidate += chrono::Duration::days(1);
    }
    Some(candidate.with_timezone(&Utc))
}

fn compute_reset_at(hour24: u32, minute: u32, tz_name: Option<&str>) -> Option<DateTime<Utc>> {
    match tz_name.and_then(|s| s.parse::<chrono_tz::Tz>().ok()) {
        Some(tz) => next_occurrence_utc(tz, hour24, minute),
        None => next_occurrence_utc(chrono::Local, hour24, minute),
    }
}

/// Pull a reset hint out of free text, trying each pattern in turn and
/// stopping at the first match: a wall-clock time (`"resets 5am"`, optionally
/// with minutes and/or an IANA zone in parens), a relative wait
/// (`"try again in 3h"`), or an HTTP `Retry-After`-style second count.
/// `resets_hint` is always the raw matched substring (for display even when
/// the absolute time below couldn't be computed); `resets_at` is best-effort
/// and `None` when nothing matched or the match couldn't be turned into a
/// timestamp.
fn parse_reset_hint(text: &str) -> (Option<String>, Option<DateTime<Utc>>) {
    if let Some(caps) = reset_clock_re().captures(text) {
        let whole = caps.get(0).map(|m| m.as_str().trim().to_string());
        let hour12: u32 = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let minute: u32 = caps
            .get(2)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let meridiem = caps.get(3).map(|m| m.as_str()).unwrap_or("am");
        let tz_name = caps.get(4).map(|m| m.as_str());
        let hour24 = to_24h(hour12, meridiem);
        return (whole, compute_reset_at(hour24, minute, tz_name));
    }
    if let Some(caps) = try_again_re().captures(text) {
        let whole = caps.get(0).map(|m| m.as_str().trim().to_string());
        let n: i64 = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        let unit = caps.get(2).map(|m| m.as_str()).unwrap_or("s");
        return (whole, Some(Utc::now() + duration_for(n, unit)));
    }
    if let Some(caps) = retry_after_re().captures(text) {
        let whole = caps.get(0).map(|m| m.as_str().trim().to_string());
        let secs: i64 = caps
            .get(1)
            .and_then(|m| m.as_str().parse().ok())
            .unwrap_or(0);
        return (whole, Some(Utc::now() + chrono::Duration::seconds(secs)));
    }
    (None, None)
}

// ─── Persistence ────────────────────────────────────────────────────────

/// Record a detected hit for `agent_type`. `model` is the connection's
/// active model at hit time (see `acp::connection::current_model_id_from_opts`),
/// supplied by the caller rather than resolved here — this module has no
/// access to `SessionState`. Ignored (dropped) for [`LimitScope::Account`],
/// where it would be misleading: the whole agent is out, not one model.
///
/// Cheap and synchronous up to the spawn; the actual read-modify-write against
/// [`KEY`] happens in a detached task so this never blocks the ACP event path
/// it is called from. A DB hiccup is logged and dropped, matching
/// `model_catalog::record_seen`.
pub fn record_hit(agent_type: AgentType, hit: LimitHit, model: Option<String>) {
    let Some(conn) = DB_CONN.get().cloned() else {
        tracing::debug!(
            agent_type = %agent_type.as_wire(),
            "model_limits: database not wired up yet, dropping a limit hit"
        );
        return;
    };
    let agent_wire = agent_type.as_wire().into_owned();
    let stored = StoredLimit {
        model: match hit.scope {
            LimitScope::Account => None,
            LimitScope::Model => model,
        },
        scope: hit.scope,
        message: hit.message,
        hit_at: Utc::now(),
        resets_hint: hit.resets_hint,
        resets_at: hit.resets_at,
    };
    tokio::spawn(async move {
        if let Err(e) = persist_hit(&conn, &agent_wire, stored).await {
            tracing::warn!(
                agent_type = %agent_wire,
                error = %e,
                "model_limits: failed to persist a limit hit"
            );
        }
    });
}

async fn read_map(conn: &DatabaseConnection) -> LimitsMap {
    match app_metadata_service::get_value(conn, KEY).await {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        _ => LimitsMap::default(),
    }
}

async fn persist_hit(
    conn: &DatabaseConnection,
    agent_wire: &str,
    stored: StoredLimit,
) -> Result<(), DbError> {
    let mut map = read_map(conn).await;
    map.insert(agent_wire.to_string(), stored);
    let json = serde_json::to_string(&map).unwrap_or_else(|_| "{}".to_string());
    app_metadata_service::upsert_value(conn, KEY, &json).await
}

/// Read every agent's current limit, already filtered to the still-active
/// ones (an expired entry is dropped here rather than eagerly swept from the
/// row — the same lazy-expiry approach as everywhere else the codebase reads
/// TTL'd state). Keyed by `AgentType::as_wire()`.
pub async fn load_all(conn: &DatabaseConnection) -> HashMap<String, StoredLimit> {
    let map = read_map(conn).await;
    let now = Utc::now();
    map.into_iter()
        .filter(|(_, v)| !v.is_expired(now))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::types::{AcpEvent as Ev, SessionFailureRecord as Rec};

    fn failure(category: &str, severity: &str, title: &str, details: Option<&str>) -> Ev {
        Ev::SessionFailure {
            record: Rec {
                id: "f1".to_string(),
                revision: 1,
                category: category.to_string(),
                severity: severity.to_string(),
                title: title.to_string(),
                details: details.map(str::to_string),
                actions: vec![],
                resolved: false,
            },
        }
    }

    fn error_event(message: &str, details: Option<&str>) -> Ev {
        Ev::Error {
            message: message.to_string(),
            agent_type: "claude_code".to_string(),
            code: None,
            details: details.map(str::to_string),
            terminal: false,
        }
    }

    // ─── detect_limit: SessionFailure ─────────────────────────────────────

    #[test]
    fn session_failure_limit_category_error_severity_is_always_a_hit() {
        let ev = failure("limit", "error", "You've hit your session limit", None);
        let hit = detect_limit(AgentType::ClaudeCode, &ev).expect("detected");
        assert_eq!(hit.scope, LimitScope::Account);
    }

    #[test]
    fn session_failure_non_limit_category_is_never_a_hit() {
        let ev = failure("connection", "error", "Lost connection to the agent", None);
        assert!(detect_limit(AgentType::ClaudeCode, &ev).is_none());
    }

    #[test]
    fn session_failure_warning_without_exhaustion_language_is_not_a_hit() {
        // A transient rate-limit warning that auto-recovers, not a spent quota.
        let ev = failure("limit", "warning", "Rate limited, retrying shortly", None);
        assert!(detect_limit(AgentType::ClaudeCode, &ev).is_none());
    }

    #[test]
    fn session_failure_warning_with_exhaustion_language_is_promoted_to_a_hit() {
        let ev = failure(
            "limit",
            "warning",
            "You have exceeded your current quota",
            None,
        );
        assert!(detect_limit(AgentType::ClaudeCode, &ev).is_some());
    }

    #[test]
    fn session_failure_falls_back_to_details_when_title_is_empty() {
        let ev = failure(
            "limit",
            "error",
            "",
            Some("usage limit reached, resets 5am"),
        );
        let hit = detect_limit(AgentType::ClaudeCode, &ev).expect("detected");
        assert_eq!(hit.scope, LimitScope::Account);
        assert!(hit.message.contains("resets 5am"));
    }

    // ─── detect_limit: Error text (Claude / OpenCode-OpenRouter / generic) ─

    #[test]
    fn claude_session_limit_text_is_detected_as_account_scope() {
        let ev = error_event("You've hit your session limit · resets 5am", None);
        let hit = detect_limit(AgentType::ClaudeCode, &ev).expect("detected");
        assert_eq!(hit.scope, LimitScope::Account);
        assert_eq!(hit.resets_hint.as_deref(), Some("resets 5am"));
    }

    #[test]
    fn openrouter_429_text_is_detected_as_model_scope() {
        let ev = error_event(
            "OpenRouter error 429: Rate limit exceeded, please try again in 20s",
            None,
        );
        let hit = detect_limit(AgentType::OpenCode, &ev).expect("detected");
        assert_eq!(hit.scope, LimitScope::Model);
        assert_eq!(hit.resets_hint.as_deref(), Some("try again in 20s"));
    }

    #[test]
    fn generic_openai_style_quota_text_is_detected() {
        let ev = error_event(
            "insufficient_quota: You exceeded your current quota, check your plan and billing details.",
            None,
        );
        let hit = detect_limit(AgentType::Codex, &ev).expect("detected");
        assert_eq!(hit.scope, LimitScope::Model);
    }

    #[test]
    fn ordinary_error_text_is_not_a_hit() {
        let ev = error_event("Failed to parse the agent's response", None);
        assert!(detect_limit(AgentType::ClaudeCode, &ev).is_none());
    }

    #[test]
    fn turn_retrying_is_never_inspected_even_with_quota_wording() {
        // Non-terminal by design (see the type's own docs) — never a hit.
        let ev = Ev::TurnRetrying {
            message: "quota exceeded, retrying".to_string(),
            error_status: Some(429),
            attempt: Some(1),
            max_retries: Some(3),
            retry_delay_ms: Some(1000),
        };
        assert!(detect_limit(AgentType::Codex, &ev).is_none());
    }

    #[test]
    fn details_are_matched_alongside_the_message() {
        let ev = error_event("Request failed", Some("429 too many requests"));
        assert!(detect_limit(AgentType::Codex, &ev).is_some());
    }

    // ─── reset-hint parsing ─────────────────────────────────────────────

    #[test]
    fn parses_a_bare_am_pm_clock_time() {
        let (hint, at) = parse_reset_hint("You've hit your limit, resets 5am");
        assert_eq!(hint.as_deref(), Some("resets 5am"));
        assert!(at.is_some());
    }

    #[test]
    fn parses_resets_at_phrasing_with_minutes() {
        let (hint, at) = parse_reset_hint("limit reached, resets at 11:30pm");
        assert_eq!(hint.as_deref(), Some("resets at 11:30pm"));
        assert!(at.is_some());
    }

    #[test]
    fn a_clock_time_already_passed_today_rolls_to_tomorrow() {
        // 00:00 has certainly already passed relative to "now" in any
        // timezone reachable from this process, so the computed instant must
        // be at least a few hours out, never in the past.
        let (_, at) = parse_reset_hint("resets 12am");
        let at = at.expect("computed a reset time");
        assert!(at > Utc::now() - chrono::Duration::minutes(1));
    }

    #[test]
    fn parses_try_again_in_hours() {
        let now = Utc::now();
        let (hint, at) = parse_reset_hint("rate limited, try again in 3h");
        assert_eq!(hint.as_deref(), Some("try again in 3h"));
        let at = at.expect("computed");
        assert!(at >= now + chrono::Duration::minutes(179));
        assert!(at <= now + chrono::Duration::minutes(181));
    }

    #[test]
    fn parses_retry_after_seconds() {
        let now = Utc::now();
        let (hint, at) = parse_reset_hint("429 Too Many Requests. Retry-After: 120");
        assert_eq!(hint.as_deref(), Some("Retry-After: 120"));
        let at = at.expect("computed");
        assert!(at >= now + chrono::Duration::seconds(115));
        assert!(at <= now + chrono::Duration::seconds(125));
    }

    #[test]
    fn no_recognizable_pattern_yields_no_hint() {
        let (hint, at) = parse_reset_hint("quota exceeded, contact support");
        assert!(hint.is_none());
        assert!(at.is_none());
    }

    // ─── expiry ───────────────────────────────────────────────────────────

    #[test]
    fn an_entry_with_a_known_reset_time_expires_exactly_then() {
        let hit_at = Utc::now() - chrono::Duration::minutes(10);
        let resets_at = Utc::now() + chrono::Duration::minutes(10);
        let entry = StoredLimit {
            scope: LimitScope::Account,
            model: None,
            message: "m".to_string(),
            hit_at,
            resets_hint: None,
            resets_at: Some(resets_at),
        };
        assert!(!entry.is_expired(Utc::now()));
        assert!(entry.is_expired(resets_at + chrono::Duration::seconds(1)));
    }

    #[test]
    fn an_entry_with_no_reset_time_expires_after_the_default_window() {
        let hit_at = Utc::now() - chrono::Duration::hours(4);
        let entry = StoredLimit {
            scope: LimitScope::Account,
            model: None,
            message: "m".to_string(),
            hit_at,
            resets_hint: None,
            resets_at: None,
        };
        assert!(!entry.is_expired(Utc::now()), "4h < the 5h default window");
        assert!(entry.is_expired(hit_at + chrono::Duration::hours(6)));
    }

    // ─── persistence round-trip ────────────────────────────────────────

    #[tokio::test]
    async fn load_all_drops_expired_entries_and_keeps_live_ones() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let live = StoredLimit {
            scope: LimitScope::Model,
            model: Some("claude-sonnet-5".to_string()),
            message: "live".to_string(),
            hit_at: Utc::now(),
            resets_hint: None,
            resets_at: Some(Utc::now() + chrono::Duration::hours(1)),
        };
        let expired = StoredLimit {
            scope: LimitScope::Account,
            model: None,
            message: "expired".to_string(),
            hit_at: Utc::now() - chrono::Duration::hours(10),
            resets_hint: None,
            resets_at: Some(Utc::now() - chrono::Duration::hours(1)),
        };
        persist_hit(&db.conn, "claude_code", live.clone())
            .await
            .expect("persist live");
        persist_hit(&db.conn, "codex", expired)
            .await
            .expect("persist expired");

        let all = load_all(&db.conn).await;
        assert_eq!(all.len(), 1);
        assert_eq!(all.get("claude_code"), Some(&live));
        assert!(!all.contains_key("codex"));
    }

    #[tokio::test]
    async fn record_hit_drops_the_model_for_account_scope() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        init(db.conn.clone());
        record_hit(
            AgentType::ClaudeCode,
            LimitHit {
                scope: LimitScope::Account,
                message: "session limit".to_string(),
                resets_hint: None,
                resets_at: None,
            },
            Some("claude-sonnet-5".to_string()),
        );
        // The write is spawned; give the runtime a turn to run it.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let all = load_all(&db.conn).await;
        let entry = all.get("claude_code").expect("recorded");
        assert_eq!(entry.scope, LimitScope::Account);
        assert!(entry.model.is_none());
    }

    #[tokio::test]
    async fn a_second_hit_for_the_same_agent_replaces_the_first_without_touching_others() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        persist_hit(
            &db.conn,
            "claude_code",
            StoredLimit {
                scope: LimitScope::Model,
                model: Some("claude-opus-5".to_string()),
                message: "first".to_string(),
                hit_at: Utc::now(),
                resets_hint: None,
                resets_at: Some(Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .await
        .expect("first persist");
        persist_hit(
            &db.conn,
            "codex",
            StoredLimit {
                scope: LimitScope::Account,
                model: None,
                message: "other agent".to_string(),
                hit_at: Utc::now(),
                resets_hint: None,
                resets_at: Some(Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .await
        .expect("second persist");
        persist_hit(
            &db.conn,
            "claude_code",
            StoredLimit {
                scope: LimitScope::Model,
                model: Some("claude-sonnet-5".to_string()),
                message: "second".to_string(),
                hit_at: Utc::now(),
                resets_hint: None,
                resets_at: Some(Utc::now() + chrono::Duration::hours(1)),
            },
        )
        .await
        .expect("third persist");

        let all = load_all(&db.conn).await;
        assert_eq!(all.len(), 2);
        assert_eq!(
            all.get("claude_code").unwrap().model.as_deref(),
            Some("claude-sonnet-5")
        );
        assert_eq!(all.get("codex").unwrap().message, "other agent");
    }
}
