//! Usage meter: "what am I burning right now", in one read.
//!
//! Two halves, deliberately independent so one failing never hides the other:
//!
//!   * **Claude quota** — the subscription's own live numbers, from
//!     `GET https://api.anthropic.com/api/oauth/usage` using the OAuth token
//!     Claude Code already stored in `~/.claude/.credentials.json`. Read-only.
//!     Any failure (no credentials, unreadable file, HTTP error, timeout,
//!     unparseable body) becomes a short `claude_error` string plus its
//!     `claude_error_kind`; the meter answers with the last good read flagged
//!     `claude_stale` (or `claude: None` if there never was one) and still
//!     answers with the token half. HTTP 429 starts a process-wide backoff
//!     ([`ClaudeUsageState`]) during which the endpoint is not called.
//!     Live rate-limit readings that claude-agent-acp attaches to
//!     `usage_update` ([`record_live_rate_limit`]) are overlaid on top of it
//!     when newer ([`overlay_live`]), so the meter stays current while the
//!     endpoint is failing.
//!   * **Token spend** — codeg's own accounting, folded from
//!     `token_usage_turn` via `db::service::token_usage_service::fetch_facts`,
//!     exactly like `commands::model_scorecard` reads them.
//!
//! **The OAuth token never reaches the wire or the log.** It is read from the
//! credentials file into a local, put in a request header, and dropped. Error
//! strings here are built from short fixed phrases plus HTTP status codes
//! only — never from the request, the response body, or the header value — so
//! `claude_error` cannot leak it either. See [`read_credentials`] for the one
//! place it exists.
//!
//! The Claude half is cached for [`CLAUDE_CACHE_TTL`] in-process
//! ([`claude_usage_cached`]): the endpoint is remote, the frontend refreshes
//! every 2 minutes, and a quota read has a natural cadence that a status-bar
//! repaint must not drive. The token half is always read fresh — it is a
//! local DB query.
//!
//! Like `commands::model_scorecard`, the aggregation is a pure function over
//! a `Vec` with the window boundaries passed in
//! ([`aggregate_usage`]), so the date logic is testable without a clock or a
//! database.
//!
//! It also carries the delegation router's profile ([`router_profile`]) and
//! the write that changes it ([`router_profile_set_core`]) — the status bar
//! shows it next to the quota it is meant to protect.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, TimeZone, Utc};
use sea_orm::DatabaseConnection;
use serde::Deserialize;

use crate::acp::delegation::router::Profile;
use crate::acp::delegation::routing::PROFILE_KEY;
use crate::app_error::AppCommandError;
use crate::commands::token_usage::MAX_SCANNED_FACTS;
use crate::db::service::app_metadata_service;
use crate::db::service::token_usage_service::{self as usage_service, FactQuery, UsageFactRow};
use crate::models::usage_meter::{
    ClaudeUsage, ClaudeUsageWindow, RouterProfileSet, TodayUsageEntry, UsageMeter,
};

/// How long a successful Claude quota read is reused before the next caller
/// goes to the network. Long enough that a burst of callers (status bar,
/// panel, `nexus uso`) shares one round trip; short enough that the number on
/// screen is never more than a minute stale.
const CLAUDE_CACHE_TTL: Duration = Duration::from_secs(60);

/// Per-request ceiling for the Anthropic call. The status bar must not hang
/// on a slow endpoint — a timed-out read surfaces as `claude_error` and the
/// panel still shows tokens.
const CLAUDE_HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// The `anthropic-beta` value the OAuth usage endpoint requires. Without it
/// the API rejects the bearer-token call.
const ANTHROPIC_OAUTH_BETA: &str = "oauth-2025-04-20";

/// Identifies Phantom's read-only quota poll to Anthropic.
const USAGE_USER_AGENT: &str = "phantom-usage/1.0";

/// Rolling window for `week_tokens`. A week of spend is the number that
/// actually informs a routing decision; the seven-day Anthropic window is
/// independent of this one and is reported separately.
const WEEK: chrono::Duration = chrono::Duration::days(7);

// ─── Public entry points ────────────────────────────────────────────────

/// Build the full usage meter.
pub async fn usage_meter_core(conn: &DatabaseConnection) -> Result<UsageMeter, AppCommandError> {
    let now = Local::now();
    let today_start = start_of_local_day(now);
    let week_start = now.with_timezone(&Utc) - WEEK;

    // One fetch covers both windows: the 7-day query is a superset of today's.
    let rows = usage_service::fetch_facts(
        conn,
        &FactQuery {
            start: Some(week_start),
            ..Default::default()
        },
        MAX_SCANNED_FACTS,
    )
    .await
    .map_err(AppCommandError::from)?;

    let (today, week_tokens) = aggregate_usage(&rows, today_start, week_start);

    let fresh = claude_usage_cached().await;
    let (now, wall_now) = (Instant::now(), Utc::now());
    let (view, failed_at, plan) = match claude_state().lock() {
        Ok(state) => (
            state.meter_view(fresh, now, wall_now),
            state.failed_at_wall(now, wall_now),
            state.last_good.as_ref().map(|u| u.plan.clone()),
        ),
        Err(_) => (
            ClaudeMeterView {
                error: fresh.is_none().then(last_claude_error),
                claude: fresh,
                ..Default::default()
            },
            None,
            None,
        ),
    };
    let live = live_rate_limits()
        .lock()
        .map(|live| live.clone())
        .unwrap_or_default();
    let (view, live_at) = overlay_live(view, &live, failed_at, plan, wall_now);

    Ok(UsageMeter {
        claude: view.claude,
        claude_error: view.error,
        claude_stale: view.stale,
        claude_error_kind: view.error_kind.map(|k| k.as_str().to_string()),
        claude_retry_at: view.retry_at.map(|at| at.to_rfc3339()),
        claude_live_at: live_at.map(|at| at.to_rfc3339()),
        today,
        week_tokens,
        router_profile: router_profile(conn).await,
    })
}

// ─── Router profile ──────────────────────────────────────────────────────

/// The delegation router's configured profile, as its own
/// `Profile::default()` when the key is absent, unreadable or unparseable.
///
/// Same resolution as `DbRouting::profile`, so the switch in the status bar
/// shows exactly what the router will do — including the default the router
/// falls back to on a bad value.
pub async fn router_profile(conn: &DatabaseConnection) -> String {
    let stored = match app_metadata_service::get_value(conn, PROFILE_KEY).await {
        Ok(Some(raw)) => raw,
        _ => String::new(),
    };
    Profile::parse(stored.trim().trim_matches('"'))
        .unwrap_or_default()
        .as_str()
        .to_string()
}

/// Validate and persist `profile`.
///
/// Validation is the router's own [`Profile::parse`], not a second copy of the
/// list: a value that reaches `app_metadata` unreadable would leave the router
/// silently on its default while the UI showed something else.
pub async fn router_profile_set_core(
    conn: &DatabaseConnection,
    profile: &str,
) -> Result<RouterProfileSet, AppCommandError> {
    let parsed = parse_router_profile(profile)?;
    app_metadata_service::upsert_value(conn, PROFILE_KEY, parsed.as_str())
        .await
        .map_err(AppCommandError::from)?;
    Ok(RouterProfileSet {
        profile: parsed.as_str().to_string(),
    })
}

/// Pure half of [`router_profile_set_core`], split out so the validation is
/// unit-testable without a database.
fn parse_router_profile(profile: &str) -> Result<Profile, AppCommandError> {
    Profile::parse(profile).ok_or_else(|| {
        AppCommandError::invalid_input(format!(
            "unknown router profile {:?}: expected one of {}",
            profile,
            Profile::ALL
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

/// The cached Claude quota, fetching it when the cache is cold or stale.
///
/// The one entry point the Claude half goes through, shared by every caller
/// (the Tauri command, the Axum handler, and the router's quota-aware
/// routing) so the 60-second window is genuinely process-wide rather than
/// per-call-site. Returns `None` on any failure and while backing off after a
/// 429 — only a fresh read is ever returned, never the last good one (that is
/// the meter's business, see [`usage_meter_core`]). The reason is left in the
/// state for [`last_claude_error`] so the caller can report it.
pub async fn claude_usage_cached() -> Option<ClaudeUsage> {
    {
        let now = Instant::now();
        if let Ok(state) = claude_state().lock() {
            if let Some(usage) = state.fresh(now) {
                return Some(usage);
            }
            if state.in_backoff(now) {
                return None;
            }
        }
    }

    let result = fetch_claude_usage().await;
    let now = Instant::now();
    match result {
        Ok(usage) => {
            if let Ok(mut state) = claude_state().lock() {
                state.record_success(usage.clone(), now);
            }
            Some(usage)
        }
        Err(err) => {
            // Only a 429 suppresses the next call. Any other failure is
            // retried on the next caller: the token may simply not be written
            // yet on a fresh install, and a long "no Claude" after the user
            // just logged in would be its own bug report.
            if let Ok(mut state) = claude_state().lock() {
                state.record_failure(err, now);
            }
            None
        }
    }
}

/// The most recent Claude-half failure message, or a generic one. Kept so
/// callers can report *why* [`claude_usage_cached`] returned `None`.
pub fn last_claude_error() -> String {
    claude_state()
        .lock()
        .ok()
        .and_then(|state| state.last_error.as_ref().map(|e| e.message.clone()))
        .unwrap_or_else(|| "claude usage unavailable".to_string())
}

// ─── Claude quota (network) ─────────────────────────────────────────────

/// `~/.claude/.credentials.json` — where Claude Code stores its OAuth token.
fn credentials_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude")
        .join(".credentials.json")
}

#[derive(Debug, Deserialize)]
struct CredentialsFile {
    #[serde(rename = "claudeAiOauth")]
    claude_ai_oauth: Option<ClaudeOauth>,
}

#[derive(Debug, Deserialize)]
struct ClaudeOauth {
    #[serde(rename = "accessToken")]
    access_token: Option<String>,
    #[serde(rename = "subscriptionType")]
    subscription_type: Option<String>,
}

/// Read the OAuth token and plan name out of the credentials file.
///
/// This is the only place the token is ever materialised. Nothing is logged,
/// returned upward, or written into an error message: the token leaves here
/// inside a request header and is dropped with the struct.
fn read_credentials() -> Result<(String, String), ClaudeFetchError> {
    read_credentials_raw().map_err(|msg| ClaudeFetchError::new(ClaudeErrorKind::Auth, msg))
}

fn read_credentials_raw() -> Result<(String, String), String> {
    let path = credentials_path();
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        // Only the error kind and the path — never the file's contents.
        format!(
            "claude credentials unreadable ({} at {})",
            io_error_kind(&e),
            path.display()
        )
    })?;
    let parsed: CredentialsFile =
        serde_json::from_str(&raw).map_err(|_| "claude credentials malformed".to_string())?;
    let oauth = parsed
        .claude_ai_oauth
        .ok_or_else(|| "claude oauth credentials absent".to_string())?;
    let token = oauth
        .access_token
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| "claude access token absent".to_string())?;
    let plan = oauth
        .subscription_type
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    Ok((token, plan))
}

/// A short, non-secret description of an IO failure. `io::Error`'s own
/// `Display` can embed the offending path in some cases, which is fine, but
/// for a file that holds a secret we prefer the bare kind.
fn io_error_kind(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => "not found",
        ErrorKind::PermissionDenied => "permission denied",
        _ => "read error",
    }
}

/// The API's quota payload. Every field is optional-with-default: Anthropic
/// adds and removes windows between plans, and an absent window must read as
/// "nothing spent", not as a parse failure that blanks the whole meter.
#[derive(Debug, Default, Deserialize)]
struct ClaudeUsageResponse {
    five_hour: Option<WindowPayload>,
    seven_day: Option<WindowPayload>,
}

#[derive(Debug, Default, Deserialize)]
struct WindowPayload {
    /// Percentage of the window consumed (`77.0` = 77%).
    utilization: Option<f64>,
    /// RFC 3339.
    resets_at: Option<String>,
}

impl From<WindowPayload> for ClaudeUsageWindow {
    fn from(p: WindowPayload) -> Self {
        ClaudeUsageWindow {
            percent: p.utilization.unwrap_or(0.0),
            resets_at: p.resets_at.filter(|s| !s.is_empty()),
        }
    }
}

/// Fold a parsed payload into the wire type. Pure — the only I/O-free half
/// of the Claude read, and the part worth a unit test.
pub(crate) fn parse_claude_usage(
    body: &str,
    plan: &str,
    fetched_at: DateTime<Utc>,
) -> Result<ClaudeUsage, String> {
    let parsed: ClaudeUsageResponse =
        serde_json::from_str(body).map_err(|_| "claude usage response malformed".to_string())?;
    Ok(ClaudeUsage {
        plan: plan.to_string(),
        five_hour: parsed.five_hour.unwrap_or_default().into(),
        seven_day: parsed.seven_day.unwrap_or_default().into(),
        fetched_at: fetched_at.to_rfc3339(),
    })
}

/// The live read: credentials in, quota out.
async fn fetch_claude_usage() -> Result<ClaudeUsage, ClaudeFetchError> {
    let (token, plan) = read_credentials()?;
    let client = match reqwest::Client::builder()
        .timeout(CLAUDE_HTTP_TIMEOUT)
        .connect_timeout(CLAUDE_HTTP_TIMEOUT)
        .user_agent(USAGE_USER_AGENT)
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return Err(ClaudeFetchError::new(
                ClaudeErrorKind::Other,
                "claude usage http client failed",
            ))
        }
    };

    let response = client
        .get("https://api.anthropic.com/api/oauth/usage")
        // The bearer value is bound into the header and never escapes.
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
        .send()
        .await
        .map_err(|e| {
            ClaudeFetchError::new(request_error_kind(&e), "claude usage request failed")
        })?;

    let status = response.status();
    if !status.is_success() {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        // Status only. The body may echo request context, and this string is
        // shipped to the frontend and the logs.
        let mut err = ClaudeFetchError::new(
            classify_http_status(status.as_u16()),
            format!("claude usage http {}", status.as_u16()),
        );
        err.retry_after = retry_after;
        return Err(err);
    }

    let body = response.text().await.map_err(|e| {
        ClaudeFetchError::new(request_error_kind(&e), "claude usage response unreadable")
    })?;
    parse_claude_usage(&body, &plan, Utc::now())
        .map_err(|msg| ClaudeFetchError::new(ClaudeErrorKind::Other, msg))
}

/// Timeouts and connection failures are "network"; anything else reqwest
/// reports is "other".
fn request_error_kind(e: &reqwest::Error) -> ClaudeErrorKind {
    if e.is_timeout() || e.is_connect() {
        ClaudeErrorKind::Network
    } else {
        ClaudeErrorKind::Other
    }
}

// ─── Failure classification and backoff (pure) ──────────────────────────

/// First 429 backoff when the response carries no usable `Retry-After`.
const CLAUDE_BACKOFF_BASE: Duration = Duration::from_secs(60);

/// Ceiling for the doubling backoff and for an honoured `Retry-After`, so a
/// bogus header cannot freeze the meter for hours.
const CLAUDE_BACKOFF_MAX: Duration = Duration::from_secs(15 * 60);

/// Coarse class of a failed Claude read, shipped as `claude_error_kind` so
/// the UI can pick its wording without parsing the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClaudeErrorKind {
    RateLimited,
    Auth,
    Network,
    Other,
}

impl ClaudeErrorKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ClaudeErrorKind::RateLimited => "rate_limited",
            ClaudeErrorKind::Auth => "auth",
            ClaudeErrorKind::Network => "network",
            ClaudeErrorKind::Other => "other",
        }
    }
}

/// A failed read: its class, a token-free message, and the server's
/// `Retry-After` when it sent one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClaudeFetchError {
    pub kind: ClaudeErrorKind,
    pub message: String,
    pub retry_after: Option<Duration>,
}

impl ClaudeFetchError {
    pub(crate) fn new(kind: ClaudeErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            retry_after: None,
        }
    }
}

pub(crate) fn classify_http_status(status: u16) -> ClaudeErrorKind {
    match status {
        429 => ClaudeErrorKind::RateLimited,
        401 | 403 => ClaudeErrorKind::Auth,
        _ => ClaudeErrorKind::Other,
    }
}

/// `Retry-After` in its delta-seconds form. The HTTP-date form is ignored
/// (falls back to the doubling backoff).
pub(crate) fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// What the meter shows for the Claude half.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct ClaudeMeterView {
    pub claude: Option<ClaudeUsage>,
    pub stale: bool,
    pub error: Option<String>,
    pub error_kind: Option<ClaudeErrorKind>,
    pub retry_at: Option<DateTime<Utc>>,
}

/// Process-wide Claude read state: the TTL cache, the last good read, the
/// last failure and the 429 backoff. Pure — every method takes `now`, so the
/// tests drive it with synthetic instants and no network.
#[derive(Debug, Default)]
pub(crate) struct ClaudeUsageState {
    /// Most recent success and when it landed; reused for [`CLAUDE_CACHE_TTL`].
    cached: Option<(Instant, ClaudeUsage)>,
    /// Most recent success, kept indefinitely for the stale display.
    last_good: Option<ClaudeUsage>,
    /// Failure of the most recent attempt; cleared by a success.
    last_error: Option<ClaudeFetchError>,
    /// When `last_error` happened; cleared by a success.
    failed_at: Option<Instant>,
    /// Consecutive 429s since the last success; drives the doubling.
    rate_limit_streak: u32,
    /// No network call before this instant.
    retry_at: Option<Instant>,
}

impl ClaudeUsageState {
    /// The cached read if it is still within the TTL.
    pub(crate) fn fresh(&self, now: Instant) -> Option<ClaudeUsage> {
        self.cached
            .as_ref()
            .filter(|(at, _)| now.saturating_duration_since(*at) < CLAUDE_CACHE_TTL)
            .map(|(_, usage)| usage.clone())
    }

    pub(crate) fn in_backoff(&self, now: Instant) -> bool {
        self.retry_at.is_some_and(|at| now < at)
    }

    pub(crate) fn record_success(&mut self, usage: ClaudeUsage, now: Instant) {
        self.cached = Some((now, usage.clone()));
        self.last_good = Some(usage);
        self.last_error = None;
        self.failed_at = None;
        self.rate_limit_streak = 0;
        self.retry_at = None;
    }

    pub(crate) fn record_failure(&mut self, err: ClaudeFetchError, now: Instant) {
        if err.kind == ClaudeErrorKind::RateLimited {
            self.rate_limit_streak = self.rate_limit_streak.saturating_add(1);
            let delay = err
                .retry_after
                .unwrap_or_else(|| doubling_backoff(self.rate_limit_streak))
                .min(CLAUDE_BACKOFF_MAX);
            self.retry_at = Some(now + delay);
        } else {
            self.retry_at = None;
        }
        self.last_error = Some(err);
        self.failed_at = Some(now);
    }

    /// Wall-clock instant of the most recent failure, if the last attempt
    /// failed. Same monotonic-to-wall mapping as `meter_view`'s `retry_at`.
    pub(crate) fn failed_at_wall(
        &self,
        now: Instant,
        wall_now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let ago =
            chrono::Duration::from_std(now.saturating_duration_since(self.failed_at?)).ok()?;
        Some(wall_now - ago)
    }

    /// The meter's answer given this round's `fresh` read (`None` when it
    /// failed or was skipped by the backoff). `wall_now` pairs with `now` to
    /// turn the monotonic retry instant into a wall-clock stamp.
    pub(crate) fn meter_view(
        &self,
        fresh: Option<ClaudeUsage>,
        now: Instant,
        wall_now: DateTime<Utc>,
    ) -> ClaudeMeterView {
        if let Some(usage) = fresh {
            return ClaudeMeterView {
                claude: Some(usage),
                ..Default::default()
            };
        }
        let retry_at = self
            .retry_at
            .filter(|at| now < *at)
            .and_then(|at| chrono::Duration::from_std(at - now).ok())
            .map(|left| wall_now + left);
        ClaudeMeterView {
            stale: self.last_good.is_some(),
            claude: self.last_good.clone(),
            error: Some(
                self.last_error
                    .as_ref()
                    .map(|e| e.message.clone())
                    .unwrap_or_else(|| "claude usage unavailable".to_string()),
            ),
            error_kind: Some(
                self.last_error
                    .as_ref()
                    .map_or(ClaudeErrorKind::Other, |e| e.kind),
            ),
            retry_at,
        }
    }
}

/// `60s · 2^(streak-1)`, capped at [`CLAUDE_BACKOFF_MAX`].
fn doubling_backoff(streak: u32) -> Duration {
    let exp = streak.saturating_sub(1).min(16);
    CLAUDE_BACKOFF_BASE
        .saturating_mul(1u32 << exp)
        .min(CLAUDE_BACKOFF_MAX)
}

fn claude_state() -> &'static Mutex<ClaudeUsageState> {
    static STATE: OnceLock<Mutex<ClaudeUsageState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(ClaudeUsageState::default()))
}

// ─── Live rate-limit feed ────────────────────────────────────────────────

/// `usage_update._meta` key under which claude-agent-acp forwards the Agent
/// SDK's `rate_limit_event` payload (`SDKRateLimitInfo`).
pub const CLAUDE_RATE_LIMIT_META_KEY: &str = "_claude/rateLimit";

/// The meter's two windows; every other `rateLimitType` is ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveWindowKind {
    FiveHour,
    SevenDay,
}

/// One live reading of a window, as received from a Claude session.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LiveWindow {
    /// `utilization` (a 0..1 fraction) as a 0..100 percent.
    pub percent: f64,
    /// RFC 3339 form of `resetsAt` (epoch seconds).
    pub resets_at: Option<String>,
    pub received_at: DateTime<Utc>,
    /// `allowed` / `allowed_warning` / `rejected`.
    pub status: String,
}

impl LiveWindow {
    fn reset_passed(&self, wall_now: DateTime<Utc>) -> bool {
        self.resets_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .is_some_and(|at| at <= wall_now)
    }

    fn to_window(&self) -> ClaudeUsageWindow {
        // A rejected request means the window is spent, whatever fraction
        // the SDK last rounded it to.
        let percent = if self.status == "rejected" {
            self.percent.max(100.0)
        } else {
            self.percent
        };
        ClaudeUsageWindow {
            percent,
            resets_at: self.resets_at.clone(),
        }
    }
}

/// Newest live reading per window, process-wide.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LiveRateLimits {
    pub five_hour: Option<LiveWindow>,
    pub seven_day: Option<LiveWindow>,
}

impl LiveRateLimits {
    /// Store `window` unless an already-newer reading holds the slot.
    pub(crate) fn record(&mut self, kind: LiveWindowKind, window: LiveWindow) {
        let slot = match kind {
            LiveWindowKind::FiveHour => &mut self.five_hour,
            LiveWindowKind::SevenDay => &mut self.seven_day,
        };
        if slot
            .as_ref()
            .is_none_or(|held| held.received_at <= window.received_at)
        {
            *slot = Some(window);
        }
    }
}

/// Parse one `SDKRateLimitInfo` object into the readings it carries.
///
/// The CLI sends one on every turn, normally as `status: "allowed"` with no
/// top-level `utilization` and both windows under `unifiedWindows`
/// (`{"five_hour": {"utilization": 0.61, "resetsAt": <epoch s>}, ...}`). The
/// top-level `rateLimitType`/`utilization` pair, when present, names the
/// window the status is about and wins for it; a `rejected` status with no
/// fraction at all still marks that window spent. Other window types,
/// non-numeric fractions and non-object values yield nothing.
pub(crate) fn parse_live_rate_limit(
    info: &serde_json::Value,
    received_at: DateTime<Utc>,
) -> Vec<(LiveWindowKind, LiveWindow)> {
    fn kind_of(name: &str) -> Option<LiveWindowKind> {
        match name {
            "five_hour" => Some(LiveWindowKind::FiveHour),
            "seven_day" => Some(LiveWindowKind::SevenDay),
            _ => None,
        }
    }
    fn percent_of(v: &serde_json::Value) -> Option<f64> {
        v.get("utilization")?
            .as_f64()
            .filter(|u| u.is_finite())
            .map(|u| (u * 100.0).clamp(0.0, 100.0))
    }
    fn resets_of(v: &serde_json::Value) -> Option<String> {
        v.get("resetsAt")
            .and_then(serde_json::Value::as_f64)
            .filter(|secs| secs.is_finite() && *secs > 0.0)
            .and_then(|secs| DateTime::from_timestamp_millis((secs * 1000.0) as i64))
            .map(|at| at.to_rfc3339())
    }

    let mut out: Vec<(LiveWindowKind, LiveWindow)> = Vec::new();
    if let Some(windows) = info.get("unifiedWindows").and_then(|w| w.as_object()) {
        for (name, window) in windows {
            if let (Some(kind), Some(percent)) = (kind_of(name), percent_of(window)) {
                out.push((
                    kind,
                    LiveWindow {
                        percent,
                        resets_at: resets_of(window),
                        received_at,
                        status: "allowed".to_string(),
                    },
                ));
            }
        }
    }

    let Some(kind) = info
        .get("rateLimitType")
        .and_then(serde_json::Value::as_str)
        .and_then(kind_of)
    else {
        return out;
    };
    let status = info
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();
    let percent = percent_of(info);
    let resets_at = resets_of(info);
    match out.iter_mut().find(|(k, _)| *k == kind) {
        Some((_, window)) => {
            if let Some(percent) = percent {
                window.percent = percent;
            }
            if resets_at.is_some() {
                window.resets_at = resets_at;
            }
            window.status = status;
        }
        None => {
            let percent = match percent {
                Some(p) => p,
                None if status == "rejected" => 100.0,
                None => return out,
            };
            out.push((
                kind,
                LiveWindow {
                    percent,
                    resets_at,
                    received_at,
                    status,
                },
            ));
        }
    }
    out
}

/// Record a live rate-limit reading (the `_claude/rateLimit` meta value of a
/// Claude Code `usage_update`). Malformed or irrelevant values are dropped.
pub fn record_live_rate_limit(info: &serde_json::Value) {
    let readings = parse_live_rate_limit(info, Utc::now());
    if readings.is_empty() {
        return;
    }
    if let Ok(mut live) = live_rate_limits().lock() {
        for (kind, window) in readings {
            live.record(kind, window);
        }
    }
}

/// Claude's quota as the rest of Phantom should act on it: the endpoint read
/// ([`claude_usage_cached`]) with the live readings overlaid, so the router
/// and the quota-resume scheduler see the same numbers as the status bar —
/// including while the endpoint is in backoff. `None` only when neither
/// source has anything.
pub async fn claude_usage_effective() -> Option<ClaudeUsage> {
    let fetched = claude_usage_cached().await;
    let plan = claude_state()
        .lock()
        .ok()
        .and_then(|state| state.last_good.as_ref().map(|u| u.plan.clone()));
    let live = live_rate_limits()
        .lock()
        .map(|live| live.clone())
        .unwrap_or_default();
    let view = ClaudeMeterView {
        claude: fetched,
        ..Default::default()
    };
    overlay_live(view, &live, None, plan, Utc::now()).0.claude
}

/// Phantom: whether either Claude window is used up (>= 100 %). Pure.
pub(crate) fn quota_exhausted(usage: &ClaudeUsage) -> bool {
    usage.five_hour.percent >= 100.0 || usage.seven_day.percent >= 100.0
}

/// Phantom: whether Claude's quota is used up right now, per
/// [`claude_usage_effective`]. A delegated child whose turn ends empty while
/// this holds was cut off by the limit, not finished.
pub async fn claude_quota_exhausted() -> bool {
    claude_usage_effective()
        .await
        .is_some_and(|usage| quota_exhausted(&usage))
}

/// Fold live readings into the meter view. Pure.
///
/// Per window, a live reading newer than `claude.fetched_at` replaces the
/// fetched numbers; with no `claude` at all, one is synthesized from the live
/// readings (`plan` = last known plan, `fetched_at` = newest reading). A
/// stale view stops being stale once a reading newer than the failed fetch
/// (`failed_at`) is shown. Readings whose `resets_at` has passed describe a
/// finished window and are skipped. Returns the view and the receipt time of
/// the newest applicable reading.
pub(crate) fn overlay_live(
    mut view: ClaudeMeterView,
    live: &LiveRateLimits,
    failed_at: Option<DateTime<Utc>>,
    plan: Option<String>,
    wall_now: DateTime<Utc>,
) -> (ClaudeMeterView, Option<DateTime<Utc>>) {
    let current = |w: &Option<LiveWindow>| w.clone().filter(|w| !w.reset_passed(wall_now));
    let five_hour = current(&live.five_hour);
    let seven_day = current(&live.seven_day);
    let Some(newest) = [&five_hour, &seven_day]
        .into_iter()
        .flatten()
        .map(|w| w.received_at)
        .max()
    else {
        return (view, None);
    };

    match view.claude.as_mut() {
        Some(usage) => {
            // An unparseable stamp cannot be proven newer than live data.
            let fetched_at = DateTime::parse_from_rfc3339(&usage.fetched_at)
                .ok()
                .map(|at| at.with_timezone(&Utc));
            for (slot, reading) in [
                (&mut usage.five_hour, &five_hour),
                (&mut usage.seven_day, &seven_day),
            ] {
                if let Some(reading) = reading {
                    if fetched_at.is_none_or(|at| reading.received_at > at) {
                        *slot = reading.to_window();
                    }
                }
            }
        }
        None => {
            view.claude = Some(ClaudeUsage {
                plan: plan.unwrap_or_default(),
                five_hour: five_hour
                    .as_ref()
                    .map(LiveWindow::to_window)
                    .unwrap_or_default(),
                seven_day: seven_day
                    .as_ref()
                    .map(LiveWindow::to_window)
                    .unwrap_or_default(),
                fetched_at: newest.to_rfc3339(),
            });
        }
    }

    if view.stale && failed_at.is_none_or(|at| newest > at) {
        view.stale = false;
    }
    (view, Some(newest))
}

fn live_rate_limits() -> &'static Mutex<LiveRateLimits> {
    static LIVE: OnceLock<Mutex<LiveRateLimits>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(LiveRateLimits::default()))
}

// ─── Token aggregation (pure) ───────────────────────────────────────────

/// Midnight of `now`'s local day, as UTC — the inclusive lower bound of the
/// "today" window.
fn start_of_local_day(now: DateTime<Local>) -> DateTime<Utc> {
    let naive = now.date_naive();
    // A local midnight that falls in a DST spring-forward gap resolves to the
    // first valid instant after the gap; `.earliest()` is never `None` for a
    // midnight in any zone the platform knows.
    naive
        .and_hms_opt(0, 0, 0)
        .and_then(|m| Local.from_local_datetime(&m).earliest())
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_else(|| now.with_timezone(&Utc))
}

/// Fold fact rows into `today` (per `(agent_type, model)`, since
/// `today_start`) and `week_tokens` (every input+output token since
/// `week_start`).
///
/// Pure: both boundaries are arguments, so a test pins them instead of
/// racing a clock. Rows outside both windows contribute nothing. Ordering is
/// deterministic — heaviest total first, then `(agent_type, model)` — so
/// repeated calls agree and the frontend can rely on row order.
pub(crate) fn aggregate_usage(
    rows: &[UsageFactRow],
    today_start: DateTime<Utc>,
    week_start: DateTime<Utc>,
) -> (Vec<TodayUsageEntry>, u64) {
    let mut week_tokens: u64 = 0;
    let mut today: Vec<TodayUsageEntry> = Vec::new();

    for row in rows {
        if row.occurred_at < week_start {
            continue;
        }
        let input = row.input_tokens.max(0) as u64;
        let output = row.output_tokens.max(0) as u64;
        week_tokens = week_tokens.saturating_add(input).saturating_add(output);

        if row.occurred_at < today_start {
            continue;
        }
        let model = row.model.clone().unwrap_or_default();
        match today
            .iter_mut()
            .find(|e| e.agent_type == row.agent_type && e.model == model)
        {
            Some(entry) => {
                entry.input_tokens = entry.input_tokens.saturating_add(input);
                entry.output_tokens = entry.output_tokens.saturating_add(output);
                entry.turns = entry.turns.saturating_add(1);
            }
            None => today.push(TodayUsageEntry {
                agent_type: row.agent_type.clone(),
                model,
                input_tokens: input,
                output_tokens: output,
                turns: 1,
            }),
        }
    }

    today.sort_by(|a, b| {
        let ta = a.input_tokens.saturating_add(a.output_tokens);
        let tb = b.input_tokens.saturating_add(b.output_tokens);
        tb.cmp(&ta)
            .then_with(|| a.agent_type.cmp(&b.agent_type))
            .then_with(|| a.model.cmp(&b.model))
    });

    (today, week_tokens)
}

// ─── Desktop command ─────────────────────────────────────────────────────

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn usage_meter(
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<UsageMeter, AppCommandError> {
    usage_meter_core(&db.conn).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn router_profile_set(
    profile: String,
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<RouterProfileSet, AppCommandError> {
    router_profile_set_core(&db.conn, &profile).await
}

#[cfg(test)]
mod tests {
    #[test]
    fn quota_is_exhausted_when_either_window_is_full() {
        let usage = |five: f64, week: f64| ClaudeUsage {
            plan: String::new(),
            five_hour: ClaudeUsageWindow {
                percent: five,
                resets_at: None,
            },
            seven_day: ClaudeUsageWindow {
                percent: week,
                resets_at: None,
            },
            fetched_at: String::new(),
        };
        assert!(!quota_exhausted(&usage(99.0, 50.0)));
        assert!(quota_exhausted(&usage(100.0, 50.0)));
        assert!(quota_exhausted(&usage(20.0, 100.0)));
    }

    use super::*;

    const SAMPLE: &str = r#"{"five_hour":{"utilization":77.0,"resets_at":"2026-10-10T11:39:59.655380+00:00"},"seven_day":{"utilization":62.0,"resets_at":"2026-10-14T13:59:59.655408+00:00"},"limits":[]}"#;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid ts")
            .with_timezone(&Utc)
    }

    fn row(agent: &str, model: Option<&str>, occurred: &str) -> UsageFactRow {
        UsageFactRow {
            conversation_id: 1,
            folder_id: 1,
            agent_type: agent.to_string(),
            model: model.map(str::to_string),
            occurred_at: ts(occurred),
            input_tokens: 100,
            output_tokens: 50,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: 150,
            duration_ms: 1000,
            tool_calls: 0,
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

    // ─── Claude payload parsing ──────────────────────────────────────────

    #[test]
    fn parses_both_windows_from_a_real_payload() {
        let usage = parse_claude_usage(SAMPLE, "pro", ts("2026-10-10T09:00:00Z")).expect("parse");
        assert_eq!(usage.plan, "pro");
        assert_eq!(usage.five_hour.percent, 77.0);
        assert_eq!(
            usage.five_hour.resets_at.as_deref(),
            Some("2026-10-10T11:39:59.655380+00:00")
        );
        assert_eq!(usage.seven_day.percent, 62.0);
        assert_eq!(
            usage.seven_day.resets_at.as_deref(),
            Some("2026-10-14T13:59:59.655408+00:00")
        );
        assert_eq!(usage.fetched_at, ts("2026-10-10T09:00:00Z").to_rfc3339());
    }

    #[test]
    fn missing_windows_and_reset_times_default_instead_of_failing() {
        // Only one window present, and its reset time is absent.
        let usage = parse_claude_usage(
            r#"{"five_hour":{"utilization":12.5}}"#,
            "max",
            ts("2026-10-10T09:00:00Z"),
        )
        .expect("parse");
        assert_eq!(usage.plan, "max");
        assert_eq!(usage.five_hour.percent, 12.5);
        assert_eq!(usage.five_hour.resets_at, None);
        assert_eq!(usage.seven_day.percent, 0.0);
        assert_eq!(usage.seven_day.resets_at, None);
    }

    #[test]
    fn an_empty_object_is_a_zeroed_meter_not_an_error() {
        let usage = parse_claude_usage("{}", "pro", ts("2026-10-10T09:00:00Z")).expect("parse");
        assert_eq!(usage.five_hour.percent, 0.0);
        assert_eq!(usage.seven_day.percent, 0.0);
    }

    #[test]
    fn a_malformed_body_is_an_error_not_a_zeroed_meter() {
        // The distinction matters: a parse failure must reach the UI as
        // `claude_error`, where a silent 0% would read as "you have quota".
        assert!(parse_claude_usage("not json", "pro", ts("2026-10-10T09:00:00Z")).is_err());
    }

    #[test]
    fn error_messages_never_carry_the_token() {
        // Belt and braces on the promise the module doc makes.
        let secret = "sk-ant-oat-SECRET";
        let body = format!(r#"{{"five_hour":{{"utilization":"{secret}"}}}}"#);
        let err = parse_claude_usage(&body, "pro", ts("2026-10-10T09:00:00Z"))
            .expect_err("utilization is not a string");
        assert!(!err.contains(secret));
    }

    // ─── Failure classification and backoff ──────────────────────────────

    fn usage(percent: f64) -> ClaudeUsage {
        ClaudeUsage {
            plan: "pro".to_string(),
            five_hour: ClaudeUsageWindow {
                percent,
                resets_at: None,
            },
            seven_day: ClaudeUsageWindow::default(),
            fetched_at: "2026-10-10T09:00:00+00:00".to_string(),
        }
    }

    fn rate_limited(retry_after: Option<u64>) -> ClaudeFetchError {
        let mut err = ClaudeFetchError::new(ClaudeErrorKind::RateLimited, "claude usage http 429");
        err.retry_after = retry_after.map(Duration::from_secs);
        err
    }

    #[test]
    fn http_statuses_are_classified() {
        assert_eq!(classify_http_status(429), ClaudeErrorKind::RateLimited);
        assert_eq!(classify_http_status(401), ClaudeErrorKind::Auth);
        assert_eq!(classify_http_status(403), ClaudeErrorKind::Auth);
        assert_eq!(classify_http_status(500), ClaudeErrorKind::Other);
        assert_eq!(classify_http_status(404), ClaudeErrorKind::Other);
        assert_eq!(ClaudeErrorKind::RateLimited.as_str(), "rate_limited");
        assert_eq!(ClaudeErrorKind::Network.as_str(), "network");
    }

    #[test]
    fn retry_after_accepts_delta_seconds_only() {
        assert_eq!(parse_retry_after(" 120 "), Some(Duration::from_secs(120)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-5"), None);
    }

    #[test]
    fn a_success_is_cached_for_the_ttl_only() {
        let t0 = Instant::now();
        let mut state = ClaudeUsageState::default();
        state.record_success(usage(10.0), t0);
        assert_eq!(state.fresh(t0 + Duration::from_secs(30)), Some(usage(10.0)));
        assert_eq!(state.fresh(t0 + CLAUDE_CACHE_TTL), None);
    }

    #[test]
    fn a_429_without_retry_after_doubles_from_60s_to_15min() {
        let t0 = Instant::now();
        let mut state = ClaudeUsageState::default();
        let mut expected = Vec::new();
        for _ in 0..7 {
            state.record_failure(rate_limited(None), t0);
            expected.push(state.retry_at.unwrap() - t0);
        }
        let secs: Vec<u64> = expected.iter().map(Duration::as_secs).collect();
        assert_eq!(secs, vec![60, 120, 240, 480, 900, 900, 900]);
        assert!(state.in_backoff(t0 + Duration::from_secs(899)));
        assert!(!state.in_backoff(t0 + Duration::from_secs(900)));
    }

    #[test]
    fn a_429_honours_retry_after_capped_at_the_ceiling() {
        let t0 = Instant::now();
        let mut state = ClaudeUsageState::default();
        state.record_failure(rate_limited(Some(30)), t0);
        assert_eq!(state.retry_at, Some(t0 + Duration::from_secs(30)));
        state.record_failure(rate_limited(Some(86_400)), t0);
        assert_eq!(state.retry_at, Some(t0 + CLAUDE_BACKOFF_MAX));
    }

    #[test]
    fn a_success_resets_the_backoff_and_the_error() {
        let t0 = Instant::now();
        let mut state = ClaudeUsageState::default();
        state.record_failure(rate_limited(None), t0);
        state.record_failure(rate_limited(None), t0);
        state.record_success(usage(5.0), t0 + Duration::from_secs(300));
        assert!(!state.in_backoff(t0 + Duration::from_secs(300)));
        assert!(state.last_error.is_none());
        // The streak restarted: the next 429 is back to 60 s.
        let t1 = t0 + Duration::from_secs(400);
        state.record_failure(rate_limited(None), t1);
        assert_eq!(state.retry_at, Some(t1 + CLAUDE_BACKOFF_BASE));
    }

    #[test]
    fn non_rate_limit_failures_do_not_back_off() {
        let t0 = Instant::now();
        let mut state = ClaudeUsageState::default();
        state.record_failure(
            ClaudeFetchError::new(ClaudeErrorKind::Network, "claude usage request failed"),
            t0,
        );
        assert!(!state.in_backoff(t0));
    }

    #[test]
    fn meter_view_serves_fresh_data_unflagged() {
        let t0 = Instant::now();
        let mut state = ClaudeUsageState::default();
        state.record_success(usage(10.0), t0);
        let view = state.meter_view(Some(usage(10.0)), t0, ts("2026-10-10T09:00:00Z"));
        assert_eq!(
            view,
            ClaudeMeterView {
                claude: Some(usage(10.0)),
                ..Default::default()
            }
        );
    }

    #[test]
    fn meter_view_serves_last_good_with_the_error_while_backing_off() {
        let t0 = Instant::now();
        let wall = ts("2026-10-10T09:00:00Z");
        let mut state = ClaudeUsageState::default();
        state.record_success(usage(42.0), t0);
        state.record_failure(rate_limited(Some(120)), t0 + Duration::from_secs(90));

        let now = t0 + Duration::from_secs(100);
        let view = state.meter_view(None, now, wall);
        assert_eq!(view.claude, Some(usage(42.0)));
        assert!(view.stale);
        assert_eq!(view.error.as_deref(), Some("claude usage http 429"));
        assert_eq!(view.error_kind, Some(ClaudeErrorKind::RateLimited));
        // 90 s + 120 s backoff, seen from t0 + 100 s: 110 s from now.
        assert_eq!(view.retry_at, Some(ts("2026-10-10T09:01:50Z")));

        // Once the backoff is over the retry stamp disappears, the data stays.
        let later = state.meter_view(None, t0 + Duration::from_secs(300), wall);
        assert!(later.stale);
        assert_eq!(later.retry_at, None);
    }

    #[test]
    fn meter_view_without_any_good_read_is_an_error_only() {
        let t0 = Instant::now();
        let mut state = ClaudeUsageState::default();
        state.record_failure(
            ClaudeFetchError::new(ClaudeErrorKind::Auth, "claude access token absent"),
            t0,
        );
        let view = state.meter_view(None, t0, ts("2026-10-10T09:00:00Z"));
        assert_eq!(view.claude, None);
        assert!(!view.stale);
        assert_eq!(view.error_kind, Some(ClaudeErrorKind::Auth));
        assert_eq!(view.retry_at, None);
    }

    // ─── Live rate-limit feed ────────────────────────────────────────────

    fn live(percent: f64, received: &str) -> LiveWindow {
        LiveWindow {
            percent,
            resets_at: Some("2026-10-10T14:00:00+00:00".to_string()),
            received_at: ts(received),
            status: "allowed".to_string(),
        }
    }

    fn live_five_hour(percent: f64, received: &str) -> LiveRateLimits {
        LiveRateLimits {
            five_hour: Some(live(percent, received)),
            seven_day: None,
        }
    }

    #[test]
    fn live_info_converts_fraction_and_epoch_seconds() {
        let info = serde_json::json!({
            "status": "allowed_warning",
            "resetsAt": 1_791_640_800u64,
            "rateLimitType": "five_hour",
            "utilization": 0.9,
            "surpassedThreshold": 0.75,
        });
        let [(kind, window)] =
            <[_; 1]>::try_from(parse_live_rate_limit(&info, ts("2026-10-10T09:00:00Z")))
                .expect("one reading");
        assert_eq!(kind, LiveWindowKind::FiveHour);
        assert!((window.percent - 90.0).abs() < 1e-9);
        assert_eq!(
            window.resets_at.as_deref(),
            Some("2026-10-10T14:00:00+00:00")
        );
        assert_eq!(window.received_at, ts("2026-10-10T09:00:00Z"));
        assert_eq!(window.status, "allowed_warning");

        let [(kind, window)] = <[_; 1]>::try_from(parse_live_rate_limit(
            &serde_json::json!({"rateLimitType": "seven_day", "utilization": 1.7}),
            ts("2026-10-10T09:00:00Z"),
        ))
        .expect("one reading");
        assert_eq!(kind, LiveWindowKind::SevenDay);
        assert_eq!(window.percent, 100.0);
        assert_eq!(window.resets_at, None);
    }

    #[test]
    fn live_info_ignores_other_windows_and_malformed_values() {
        let at = ts("2026-10-10T09:00:00Z");
        for info in [
            serde_json::json!({"rateLimitType": "seven_day_opus", "utilization": 0.5}),
            serde_json::json!({"rateLimitType": "overage", "utilization": 0.5}),
            serde_json::json!({"rateLimitType": "five_hour"}),
            serde_json::json!({"rateLimitType": "five_hour", "status": "allowed"}),
            serde_json::json!({"rateLimitType": "five_hour", "utilization": "0.5"}),
            serde_json::json!({"unifiedWindows": {"five_hour": {"utilization": "x"}}}),
            serde_json::json!({"utilization": 0.5}),
            serde_json::json!("five_hour"),
            serde_json::Value::Null,
        ] {
            assert_eq!(parse_live_rate_limit(&info, at), vec![], "{info}");
        }
    }

    #[test]
    fn live_info_reads_unified_windows_from_a_plain_allowed_event() {
        // Shape captured from claude 2.1.x `rate_limit_event` (stream-json).
        let info = serde_json::json!({
            "status": "allowed",
            "resetsAt": 1_792_736_400u64,
            "rateLimitType": "five_hour",
            "overageStatus": "rejected",
            "isUsingOverage": false,
            "unifiedWindows": {
                "five_hour": {"utilization": 0.61, "resetsAt": 1_792_736_400u64},
                "seven_day": {"utilization": 0.25, "resetsAt": 1_793_196_000u64},
            },
        });
        let mut readings = parse_live_rate_limit(&info, ts("2026-10-23T03:40:00Z"));
        readings.sort_by_key(|(k, _)| *k == LiveWindowKind::SevenDay);
        let [(k5, w5), (k7, w7)] = <[_; 2]>::try_from(readings).expect("two readings");
        assert_eq!(
            (k5, k7),
            (LiveWindowKind::FiveHour, LiveWindowKind::SevenDay)
        );
        assert!((w5.percent - 61.0).abs() < 1e-9);
        assert!((w7.percent - 25.0).abs() < 1e-9);
        assert_eq!(w5.status, "allowed");
        assert!(w7.resets_at.is_some());
    }

    #[test]
    fn a_rejected_event_without_a_fraction_marks_its_window_spent() {
        // Shape recorded in Claude Code transcripts on a 429.
        let info = serde_json::json!({
            "status": "rejected",
            "resetsAt": 1_791_697_800u64,
            "rateLimitType": "five_hour",
            "overageStatus": "rejected",
            "isUsingOverage": false,
        });
        let [(kind, window)] =
            <[_; 1]>::try_from(parse_live_rate_limit(&info, ts("2026-10-10T09:00:00Z")))
                .expect("one reading");
        assert_eq!(kind, LiveWindowKind::FiveHour);
        assert_eq!(window.to_window().percent, 100.0);
    }

    #[test]
    fn an_older_live_reading_does_not_replace_a_newer_one() {
        let mut limits = LiveRateLimits::default();
        limits.record(LiveWindowKind::FiveHour, live(50.0, "2026-10-10T09:05:00Z"));
        limits.record(LiveWindowKind::FiveHour, live(40.0, "2026-10-10T09:01:00Z"));
        assert_eq!(limits.five_hour.as_ref().map(|w| w.percent), Some(50.0));
        limits.record(LiveWindowKind::FiveHour, live(55.0, "2026-10-10T09:06:00Z"));
        assert_eq!(limits.five_hour.as_ref().map(|w| w.percent), Some(55.0));
        assert_eq!(limits.seven_day, None);
    }

    #[test]
    fn newer_live_overrides_its_window_only() {
        // `usage()` was fetched at 09:00.
        let view = ClaudeMeterView {
            claude: Some(usage(10.0)),
            ..Default::default()
        };
        let (view, live_at) = overlay_live(
            view,
            &live_five_hour(64.0, "2026-10-10T09:02:00Z"),
            None,
            None,
            ts("2026-10-10T09:03:00Z"),
        );
        let claude = view.claude.expect("claude");
        assert_eq!(claude.five_hour.percent, 64.0);
        assert_eq!(
            claude.five_hour.resets_at.as_deref(),
            Some("2026-10-10T14:00:00+00:00")
        );
        assert_eq!(claude.seven_day, ClaudeUsageWindow::default());
        assert_eq!(claude.fetched_at, "2026-10-10T09:00:00+00:00");
        assert_eq!(live_at, Some(ts("2026-10-10T09:02:00Z")));
    }

    #[test]
    fn older_live_is_ignored() {
        let view = ClaudeMeterView {
            claude: Some(usage(10.0)),
            ..Default::default()
        };
        let (view, _) = overlay_live(
            view,
            &live_five_hour(64.0, "2026-10-10T08:59:00Z"),
            None,
            None,
            ts("2026-10-10T09:03:00Z"),
        );
        assert_eq!(view.claude.expect("claude").five_hour.percent, 10.0);
    }

    #[test]
    fn live_from_a_finished_window_is_ignored() {
        let view = ClaudeMeterView {
            claude: Some(usage(10.0)),
            ..Default::default()
        };
        let (view, live_at) = overlay_live(
            view,
            &live_five_hour(64.0, "2026-10-10T09:02:00Z"),
            None,
            None,
            ts("2026-10-10T14:00:01Z"),
        );
        assert_eq!(view.claude.expect("claude").five_hour.percent, 10.0);
        assert_eq!(live_at, None);
    }

    #[test]
    fn a_rejected_live_reading_reads_as_spent() {
        let mut limits = live_five_hour(98.0, "2026-10-10T09:02:00Z");
        limits.five_hour.as_mut().unwrap().status = "rejected".to_string();
        let (view, _) = overlay_live(
            ClaudeMeterView::default(),
            &limits,
            None,
            None,
            ts("2026-10-10T09:03:00Z"),
        );
        assert_eq!(view.claude.expect("claude").five_hour.percent, 100.0);
    }

    #[test]
    fn live_without_any_fetch_synthesizes_the_quota() {
        let limits = LiveRateLimits {
            five_hour: Some(live(30.0, "2026-10-10T09:01:00Z")),
            seven_day: Some(live(70.0, "2026-10-10T09:02:00Z")),
        };
        let view = ClaudeMeterView {
            error: Some("claude usage http 429".to_string()),
            error_kind: Some(ClaudeErrorKind::RateLimited),
            ..Default::default()
        };
        let (view, live_at) = overlay_live(view, &limits, None, None, ts("2026-10-10T09:03:00Z"));
        let claude = view.claude.expect("synthesized");
        assert_eq!(claude.plan, "");
        assert_eq!(claude.five_hour.percent, 30.0);
        assert_eq!(claude.seven_day.percent, 70.0);
        assert_eq!(claude.fetched_at, ts("2026-10-10T09:02:00Z").to_rfc3339());
        assert!(!view.stale);
        assert_eq!(live_at, Some(ts("2026-10-10T09:02:00Z")));

        let (view, _) = overlay_live(
            ClaudeMeterView::default(),
            &limits,
            None,
            Some("max".to_string()),
            ts("2026-10-10T09:03:00Z"),
        );
        assert_eq!(view.claude.expect("synthesized").plan, "max");
    }

    #[test]
    fn no_live_data_leaves_the_view_untouched() {
        let view = ClaudeMeterView {
            claude: Some(usage(10.0)),
            stale: true,
            ..Default::default()
        };
        let (out, live_at) = overlay_live(
            ClaudeMeterView {
                claude: Some(usage(10.0)),
                stale: true,
                ..Default::default()
            },
            &LiveRateLimits::default(),
            None,
            None,
            ts("2026-10-10T09:03:00Z"),
        );
        assert_eq!(out, view);
        assert_eq!(live_at, None);
    }

    #[test]
    fn live_newer_than_the_failed_fetch_clears_stale() {
        let t0 = Instant::now();
        let wall0 = ts("2026-10-10T09:00:00Z");
        let mut state = ClaudeUsageState::default();
        state.record_success(usage(42.0), t0);
        // 429 at 09:05.
        state.record_failure(rate_limited(Some(600)), t0 + Duration::from_secs(300));
        let now = t0 + Duration::from_secs(420);
        let wall_now = wall0 + chrono::Duration::seconds(420);
        let failed_at = state.failed_at_wall(now, wall_now);
        assert_eq!(failed_at, Some(ts("2026-10-10T09:05:00Z")));

        // A reading between the last good fetch and the 429: shown, still stale.
        let (view, _) = overlay_live(
            state.meter_view(None, now, wall_now),
            &live_five_hour(50.0, "2026-10-10T09:04:00Z"),
            failed_at,
            None,
            wall_now,
        );
        assert_eq!(view.claude.as_ref().unwrap().five_hour.percent, 50.0);
        assert!(view.stale);

        // A reading after the 429: current numbers, no longer stale.
        let (view, _) = overlay_live(
            state.meter_view(None, now, wall_now),
            &live_five_hour(55.0, "2026-10-10T09:06:00Z"),
            failed_at,
            None,
            wall_now,
        );
        assert_eq!(view.claude.as_ref().unwrap().five_hour.percent, 55.0);
        assert!(!view.stale);
        assert_eq!(view.error_kind, Some(ClaudeErrorKind::RateLimited));
    }

    #[test]
    fn a_success_clears_the_failure_instant() {
        let t0 = Instant::now();
        let wall = ts("2026-10-10T09:00:00Z");
        let mut state = ClaudeUsageState::default();
        state.record_failure(rate_limited(None), t0);
        assert!(state.failed_at_wall(t0, wall).is_some());
        state.record_success(usage(1.0), t0);
        assert_eq!(state.failed_at_wall(t0, wall), None);
    }

    // ─── Router profile validation ───────────────────────────────────────

    #[test]
    fn both_profiles_are_accepted() {
        assert_eq!(parse_router_profile("calidad").unwrap(), Profile::Calidad);
        assert_eq!(
            parse_router_profile("equilibrado").unwrap(),
            Profile::Equilibrado
        );
        // `Profile::parse` trims and lowercases, so the command is forgiving
        // about casing and padding but not about the value itself.
        assert_eq!(
            parse_router_profile(" Equilibrado ").unwrap(),
            Profile::Equilibrado
        );
    }

    #[test]
    fn an_unknown_profile_is_rejected_and_lists_the_valid_ones() {
        for bad in [
            "",
            "  ",
            "rapido",
            "quality",
            "calidad barato",
            "calidad\tx",
        ] {
            let err = parse_router_profile(bad).expect_err("must reject");
            assert!(
                matches!(err.code, crate::app_error::AppErrorCode::InvalidInput),
                "{:?}",
                err.code
            );
            assert!(err.message.contains("calidad"), "{}", err.message);
            assert!(err.message.contains("equilibrado"), "{}", err.message);
        }
    }

    #[test]
    fn a_rejected_profile_is_not_silently_replaced_by_the_default() {
        // The failure mode this guards: storing an unreadable value would
        // leave the router on `calidad` while the UI showed something else.
        assert!(parse_router_profile("turbo").is_err());
    }

    // ─── Token aggregation ───────────────────────────────────────────────

    #[test]
    fn aggregates_today_by_agent_and_model() {
        // Window boundaries are UTC instants standing in for "local midnight"
        // and "7 days ago", so this test never reads a clock.
        let today_start = ts("2026-10-10T00:00:00Z");
        let week_start = ts("2026-10-03T00:00:00Z");

        let mut a1 = row(
            "claude_code",
            Some("claude-sonnet-5"),
            "2026-10-10T10:00:00Z",
        );
        a1.input_tokens = 1000;
        a1.output_tokens = 100;
        let a2 = row(
            "claude_code",
            Some("claude-sonnet-5"),
            "2026-10-10T11:00:00Z",
        );
        let mut b1 = row(
            "open_code",
            Some("space-bunny-free"),
            "2026-10-10T12:00:00Z",
        );
        b1.input_tokens = 50;
        b1.output_tokens = 25;

        let (today, _) = aggregate_usage(&[a1, a2, b1], today_start, week_start);

        assert_eq!(today.len(), 2);
        // Heaviest first.
        assert_eq!(today[0].agent_type, "claude_code");
        assert_eq!(today[0].model, "claude-sonnet-5");
        assert_eq!(today[0].input_tokens, 1100);
        assert_eq!(today[0].output_tokens, 150);
        assert_eq!(today[0].turns, 2);
        assert_eq!(today[1].agent_type, "open_code");
        assert_eq!(today[1].input_tokens, 50);
        assert_eq!(today[1].turns, 1);
    }

    #[test]
    fn week_tokens_span_the_window_and_today_stops_at_midnight() {
        let today_start = ts("2026-10-10T00:00:00Z");
        let week_start = ts("2026-10-03T00:00:00Z");

        let old = row("claude_code", Some("m"), "2026-10-05T10:00:00Z"); // week only
        let yesterday = row("claude_code", Some("m"), "2026-10-09T23:00:00Z"); // week only
        let now_row = row("claude_code", Some("m"), "2026-10-10T09:00:00Z"); // both

        let (today, week) = aggregate_usage(&[old, yesterday, now_row], today_start, week_start);

        // 3 turns x 150 tokens.
        assert_eq!(week, 450);
        // Only the post-midnight turn.
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].turns, 1);
        assert_eq!(today[0].input_tokens, 100);
    }

    #[test]
    fn rows_older_than_the_week_are_dropped_entirely() {
        let week_start = ts("2026-10-03T00:00:00Z");
        let (today, week) = aggregate_usage(
            &[row("claude_code", Some("m"), "2026-09-01T10:00:00Z")],
            ts("2026-10-10T00:00:00Z"),
            week_start,
        );
        assert_eq!(week, 0);
        assert!(today.is_empty());
    }

    #[test]
    fn a_turn_with_no_recorded_model_is_still_attributed_to_its_agent() {
        let today_start = ts("2026-10-10T00:00:00Z");
        let (today, _) = aggregate_usage(
            &[row("claude_code", None, "2026-10-10T09:00:00Z")],
            today_start,
            ts("2026-10-03T00:00:00Z"),
        );
        assert_eq!(today.len(), 1);
        assert_eq!(today[0].model, "");
        assert_eq!(today[0].agent_type, "claude_code");
    }

    #[test]
    fn an_empty_history_produces_an_empty_meter_not_an_error() {
        let (today, week) =
            aggregate_usage(&[], ts("2026-10-10T00:00:00Z"), ts("2026-10-03T00:00:00Z"));
        assert!(today.is_empty());
        assert_eq!(week, 0);
    }

    #[test]
    fn start_of_local_day_is_midnight_local() {
        let now = Local
            .with_ymd_and_hms(2026, 10, 10, 15, 30, 0)
            .earliest()
            .expect("valid local time");
        let start = start_of_local_day(now).with_timezone(&Local);
        assert_eq!(
            start,
            Local
                .with_ymd_and_hms(2026, 10, 10, 0, 0, 0)
                .earliest()
                .expect("valid local midnight")
        );
        assert!(start < now.with_timezone(&Utc));
    }
}
