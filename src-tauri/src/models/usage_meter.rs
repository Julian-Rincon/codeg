//! Wire types for the usage meter — the "what am I burning right now" read.
//!
//! Two halves with different natures:
//!   * `claude` — the subscription's *live* quota, read from Anthropic's
//!     OAuth usage endpoint using the token Claude Code already stored in
//!     `~/.claude/.credentials.json`. Read-only, cached; on a failed read it
//!     is the last good value flagged `claude_stale`, or absent if there is
//!     none (see `claude_error`).
//!   * `today` / `week_tokens` — codeg's own accounting, folded from
//!     `token_usage_turn`. Always present, because codeg's own DB always
//!     answers.
//!
//! Field names here are load-bearing wire contract — the status bar segment
//! and the `nexus uso` CLI are built against this exact snake_case shape —
//! so nothing is renamed and nothing is skipped when `None`: every field
//! always appears in the JSON, as either a value or `null`.
//!
//! See `commands::usage_meter` for how every field is computed.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct UsageMeter {
    /// Anthropic quota. The fresh read when it succeeded; otherwise the last
    /// successful read (`claude_stale: true`), or `None` if there never was
    /// one. The failure reason is in `claude_error` (never the token — see
    /// that module).
    pub claude: Option<ClaudeUsage>,
    pub claude_error: Option<String>,
    /// `true` when `claude` is the last good read rather than a fresh one.
    pub claude_stale: bool,
    /// Class of `claude_error`: `rate_limited`, `auth`, `network` or `other`.
    pub claude_error_kind: Option<String>,
    /// RFC 3339 instant before which the endpoint is not called again (set
    /// only while backing off after HTTP 429).
    pub claude_retry_at: Option<String>,
    /// RFC 3339 receipt time of the newest live rate-limit reading
    /// (claude-agent-acp's `_claude/rateLimit` on `usage_update`) folded into
    /// `claude`, or `None` when no live reading applies.
    pub claude_live_at: Option<String>,
    /// Today's tokens per `(agent_type, model)`, heaviest first.
    pub today: Vec<TodayUsageEntry>,
    /// Total input + output tokens over the last 7 days, all agents.
    pub week_tokens: u64,
    /// Configured delegation-router profile (`calidad` / `equilibrado`), read
    /// from `phantom.router.profile`. Never absent: an unset or unreadable key
    /// reads as the router's own default.
    pub router_profile: String,
}

/// Answer of `router_profile_set`: the profile now stored, echoed back so the
/// frontend can trust what was written rather than assuming it.
#[derive(Debug, Clone, Serialize)]
pub struct RouterProfileSet {
    pub profile: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClaudeUsage {
    /// `subscriptionType` from the credentials file (`pro`, `max`, …).
    pub plan: String,
    pub five_hour: ClaudeUsageWindow,
    pub seven_day: ClaudeUsageWindow,
    /// RFC 3339 stamp of when these numbers were fetched from Anthropic.
    pub fetched_at: String,
}

/// One quota window: how much of it is spent, and when it resets.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ClaudeUsageWindow {
    /// Percent of the window consumed, as reported (`77.0` = 77%). Defaults
    /// to `0.0` when the window is missing from the response — a fresh
    /// account with no consumption is exactly that, and a genuinely absent
    /// window is the same display ("nothing spent").
    pub percent: f64,
    /// RFC 3339 reset instant, verbatim from the API. `None` when the API
    /// omitted it.
    pub resets_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TodayUsageEntry {
    pub agent_type: String,
    /// Empty string when the turn recorded no model — kept rather than
    /// dropped, so the tokens are still attributed to the agent that burned
    /// them.
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turns: u64,
}
