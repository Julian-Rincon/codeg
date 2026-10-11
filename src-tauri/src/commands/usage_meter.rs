//! Usage meter: "what am I burning right now", in one read.
//!
//! Two halves, deliberately independent so one failing never hides the other:
//!
//!   * **Claude quota** — the subscription's own live numbers, from
//!     `GET https://api.anthropic.com/api/oauth/usage` using the OAuth token
//!     Claude Code already stored in `~/.claude/.credentials.json`. Read-only.
//!     Any failure (no credentials, unreadable file, HTTP error, timeout,
//!     unparseable body) becomes `claude: None` plus a short `claude_error`
//!     string — the meter still answers with the token half.
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

    let (claude, claude_error) = match claude_usage_cached().await {
        Some(usage) => (Some(usage), None),
        None => (None, Some(last_claude_error())),
    };

    Ok(UsageMeter {
        claude,
        claude_error,
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
/// per-call-site. Returns `None` on any failure; the reason is left in the
/// error slot for [`last_claude_error`] so the caller can report it.
pub async fn claude_usage_cached() -> Option<ClaudeUsage> {
    {
        let cache = claude_cache();
        if let Ok(guard) = cache.lock() {
            if let Some((fetched_at, usage)) = guard.as_ref() {
                if fetched_at.elapsed() < CLAUDE_CACHE_TTL {
                    return Some(usage.clone());
                }
            }
        }
    }

    let usage = match fetch_claude_usage().await {
        Ok(usage) => usage,
        Err(msg) => {
            // A failure is NOT cached: the token may simply not be written
            // yet on a fresh install, and a one-minute-long "no Claude" after
            // the user just logged in would be its own bug report.
            record_claude_error(msg);
            return None;
        }
    };

    if let Ok(mut guard) = claude_cache().lock() {
        *guard = Some((Instant::now(), usage.clone()));
    }
    Some(usage)
}

/// The most recent Claude-half failure message, or a generic one. Kept so
/// `usage_meter_core` can report *why* `claude` is `null` even though
/// [`claude_usage_cached`] only returns the value.
pub fn last_claude_error() -> String {
    claude_error_slot()
        .lock()
        .ok()
        .and_then(|guard| guard.clone())
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
fn read_credentials() -> Result<(String, String), String> {
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
        five_hour: parsed
            .five_hour
            .unwrap_or_default()
            .into(),
        seven_day: parsed
            .seven_day
            .unwrap_or_default()
            .into(),
        fetched_at: fetched_at.to_rfc3339(),
    })
}

/// The live read: credentials in, quota out.
async fn fetch_claude_usage() -> Result<ClaudeUsage, String> {
    let (token, plan) = read_credentials()?;
    let client = match reqwest::Client::builder()
        .timeout(CLAUDE_HTTP_TIMEOUT)
        .connect_timeout(CLAUDE_HTTP_TIMEOUT)
        .user_agent(USAGE_USER_AGENT)
        .build()
    {
        Ok(c) => c,
        Err(_) => return Err("claude usage http client failed".to_string()),
    };

    let response = client
        .get("https://api.anthropic.com/api/oauth/usage")
        // The bearer value is bound into the header and never escapes.
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header("anthropic-beta", ANTHROPIC_OAUTH_BETA)
        .send()
        .await
        .map_err(|_| "claude usage request failed".to_string())?;

    let status = response.status();
    if !status.is_success() {
        // Status only. The body may echo request context, and this string is
        // shipped to the frontend and the logs.
        return Err(format!("claude usage http {}", status.as_u16()));
    }

    let body = response
        .text()
        .await
        .map_err(|_| "claude usage response unreadable".to_string())?;
    parse_claude_usage(&body, &plan, Utc::now())
}

// ─── Caching ────────────────────────────────────────────────────────────

/// Last successful read plus the instant it landed.
type ClaudeCache = Mutex<Option<(Instant, ClaudeUsage)>>;

fn claude_cache() -> &'static ClaudeCache {
    static CACHE: OnceLock<ClaudeCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn claude_error_slot() -> &'static Mutex<Option<String>> {
    static SLOT: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn record_claude_error(msg: String) {
    if let Ok(mut guard) = claude_error_slot().lock() {
        *guard = Some(msg);
    }
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
        let usage =
            parse_claude_usage(SAMPLE, "pro", ts("2026-10-10T09:00:00Z")).expect("parse");
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
        assert_eq!(parse_router_profile(" Equilibrado ").unwrap(), Profile::Equilibrado);
    }

    #[test]
    fn an_unknown_profile_is_rejected_and_lists_the_valid_ones() {
        for bad in ["", "  ", "rapido", "quality", "calidad barato", "calidad\tx"] {
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

        let mut a1 = row("claude_code", Some("claude-sonnet-5"), "2026-10-10T10:00:00Z");
        a1.input_tokens = 1000;
        a1.output_tokens = 100;
        let a2 = row("claude_code", Some("claude-sonnet-5"), "2026-10-10T11:00:00Z");
        let mut b1 = row("open_code", Some("space-bunny-free"), "2026-10-10T12:00:00Z");
        b1.input_tokens = 50;
        b1.output_tokens = 25;

        let (today, _) =
            aggregate_usage(&[a1, a2, b1], today_start, week_start);

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
        let (today, week) = aggregate_usage(
            &[],
            ts("2026-10-10T00:00:00Z"),
            ts("2026-10-03T00:00:00Z"),
        );
        assert!(today.is_empty());
        assert_eq!(week, 0);
    }

    #[test]
    fn start_of_local_day_is_midnight_local() {
        let now = Local.with_ymd_and_hms(2026, 10, 10, 15, 30, 0)
            .earliest()
            .expect("valid local time");
        let start = start_of_local_day(now).with_timezone(&Local);
        assert_eq!(
            start,
            Local.with_ymd_and_hms(2026, 10, 10, 0, 0, 0)
                .earliest()
                .expect("valid local midnight")
        );
        assert!(start < now.with_timezone(&Utc));
    }
}