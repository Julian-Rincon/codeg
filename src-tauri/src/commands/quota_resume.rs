//! "Esperar y continuar solo": schedule a conversation to be resumed by itself
//! when its agent's quota comes back.
//!
//! The user presses *Esperar* on a limit card; Phantom writes one pending entry
//! into `app_metadata` (`phantom.quota_resume`), and a background loop wakes
//! every 30 s to resume whatever has come due. The registry is persisted rather
//! than held in memory because the whole point of the feature is that it
//! **survives a restart of Phantom** — the user is told "it will continue by
//! itself", so losing the schedule on a crash would be a lie.
//!
//! ## Layering
//!
//! The interesting decisions are all pure and take `now` as an argument, so
//! they are unit-tested against a fake clock ([`pick_resume_at`],
//! [`due_indices`], [`apply_outcome`], [`prune_entries`], [`upsert_pending],
//! [`scheduled_entries_view]). Everything that touches the world — the database,
//! the ACP connection, the Telegram notice — lives in a thin shell on top:
//! [`schedule_core`], [`list_core`], [`cancel_core`] and [`process_due`].
//!
//! The shell is still testable because both of its edges are traits
//! ([`ResumeEffects`]): a test supplies a fake transport over an in-memory
//! SQLite database and never opens a socket, spawns a process, or notifies.
//!
//! ## Which reset time wins
//!
//! For `claude_code` the authoritative source is Claude's own quota endpoint
//! (via `commands::usage_meter::claude_usage_cached`), read only when a window
//! is actually saturated — a half-empty window's reset time says nothing about
//! when the *limit* lifts. Otherwise the reset time recorded by
//! [`acp::model_limits`] for that agent is used. With neither, the schedule
//! falls back to five hours out rather than firing immediately, so a limit
//! whose reset time could not be parsed still resumes rather than never.
//!
//! [`acp::model_limits`]: crate::acp::model_limits

use std::sync::OnceLock;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use crate::acp::manager::ConnectionManager;
use crate::acp::model_limits;
use crate::acp::types::{ConnectionStatus, PromptInputBlock};
use crate::app_error::AppCommandError;
use crate::db::service::app_metadata_service;
use crate::models::AgentType;

/// The single `app_metadata` row holding the JSON list of scheduled resumes.
pub const KEY: &str = "phantom.quota_resume";

/// Hard cap on persisted entries. Finished entries are dropped oldest-first
/// first, so the cap only ever bites on a pathological number of simultaneously
/// pending schedules.
pub const MAX_ENTRIES: usize = 50;

/// How often the background loop looks for work. One minute of slack around the
/// nominal reset time is cheaper than a tighter poll nobody can perceive.
pub const TICK: Duration = Duration::from_secs(30);

/// Grace period added to every reset time. Anthropic's reset stamp is the
/// instant the window *rolls over*; resuming on the dot is a coin flip against
/// a limit that may still be counted for a few more seconds.
const RESET_BUFFER: chrono::Duration = chrono::Duration::seconds(60);

/// Used when no reset time could be found anywhere. Matches the expiry
/// `acp::model_limits` already assumes for a hit with no parseable reset.
const FALLBACK_DELAY: chrono::Duration = chrono::Duration::hours(5);

/// A window at or above this percentage is treated as spent.
const SATURATED_PERCENT: f64 = 99.0;

/// Backoff between resume attempts after a failed send.
const RETRY_DELAY: chrono::Duration = chrono::Duration::minutes(10);

/// Attempts after which a schedule is given up on.
const MAX_ATTEMPTS: u32 = 3;

/// The prompt sent to the conversation when its quota comes back. Fixed
/// wording: the user asked for "continue where you left off", and an agent
/// needs no more than that to resume its own context.
pub const RESUME_PROMPT: &str = "La cuota se recuperó: continúa exactamente donde quedaste.";

// ─── Types ──────────────────────────────────────────────────────────────

/// Lifecycle of one scheduled resume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeStatus {
    /// Waiting for `resume_at`.
    Pending,
    /// The prompt was handed to a live connection.
    Done,
    /// The user cancelled it, or rescheduled it elsewhere.
    Canceled,
    /// The connection was no longer open when the time came.
    ConnectionGone,
    /// `MAX_ATTEMPTS` sends all failed.
    Failed,
}

impl ResumeStatus {
    /// True only while the loop still has work to do for this entry.
    pub fn is_pending(self) -> bool {
        matches!(self, ResumeStatus::Pending)
    }
}

/// One persisted schedule. Serialized as the JSON list under [`KEY`]; the field
/// names are that file's format, not a free choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaResumeEntry {
    pub id: String,
    pub connection_id: String,
    pub conversation_id: i32,
    /// `AgentType::as_wire()`, stored as a plain string so the row never has
    /// to be re-parsed when a new agent variant is added to the enum.
    pub agent_type: String,
    /// When to fire, RFC 3339 via chrono's serde.
    pub resume_at: DateTime<Utc>,
    pub status: ResumeStatus,
    pub attempts: u32,
    pub created_at: DateTime<Utc>,
}

/// Result of [`schedule_core`].
#[derive(Debug, Clone, Serialize)]
pub struct QuotaResumeScheduled {
    pub id: String,
    pub resume_at: DateTime<Utc>,
}

/// One row of `quota_resume_list`. Deliberately narrower than
/// [`QuotaResumeEntry`]: this is display state, and the connection id is an
/// implementation detail the UI has no use for.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaResumeView {
    pub id: String,
    pub conversation_id: i32,
    pub agent_type: String,
    pub resume_at: DateTime<Utc>,
    pub status: ResumeStatus,
    pub attempts: u32,
}

/// Result of [`cancel_core`].
#[derive(Debug, Clone, Serialize)]
pub struct QuotaResumeCanceled {
    pub ok: bool,
}

/// What the loop observed when it tried to resume one entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeOutcome {
    Sent,
    ConnectionGone,
    SendFailed,
}

/// Build the id of a new entry. Prefixed so an id is recognizable in a log line
/// or a support request, and short enough to paste into a URL.
fn new_id() -> String {
    let simple = uuid::Uuid::new_v4().simple().to_string();
    format!("qr-{}", &simple[..12])
}

// ─── Pure decision logic ────────────────────────────────────────────────

/// Parse an RFC 3339 stamp into UTC. `None` for anything unparseable, which
/// the caller treats as "this source has no reset time".
fn parse_ts(raw: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Claude's own reset time, but only from a window that is actually spent: the
/// five-hour window when it is saturated, otherwise the weekly one.
fn claude_reset(claude: &crate::models::usage_meter::ClaudeUsage) -> Option<DateTime<Utc>> {
    for window in [&claude.five_hour, &claude.seven_day] {
        if window.percent >= SATURATED_PERCENT {
            if let Some(at) = parse_ts(window.resets_at.as_deref()) {
                return Some(at);
            }
        }
    }
    None
}

/// The instant a schedule should fire.
///
/// `claude` is consulted only for `claude_code` — another agent's quota has
/// nothing to do with when Claude's window rolls over. `limit_resets_at` is the
/// `acp::model_limits` entry for the same agent. With neither, the fallback
/// keeps the promise ("it continues by itself") instead of firing instantly,
/// which would just fail and burn an attempt.
pub(crate) fn pick_resume_at(
    now: DateTime<Utc>,
    agent_type: &str,
    claude: Option<&crate::models::usage_meter::ClaudeUsage>,
    limit_resets_at: Option<DateTime<Utc>>,
) -> DateTime<Utc> {
    let base = if agent_type == AgentType::ClaudeCode.as_wire() {
        claude.and_then(claude_reset)
    } else {
        None
    }
    .or(limit_resets_at)
    .unwrap_or_else(|| now + FALLBACK_DELAY);
    base + RESET_BUFFER
}

/// Indices of the entries the loop must act on at `now`: pending and due.
/// Both halves matter — a finished entry with a past `resume_at` would
/// otherwise be "resumed" again forever.
pub(crate) fn due_indices(entries: &[QuotaResumeEntry], now: DateTime<Utc>) -> Vec<usize> {
    entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.status.is_pending() && e.resume_at <= now)
        .map(|(i, _)| i)
        .collect()
}

/// Apply one attempt's outcome to `entry`, returning the Telegram text to send
/// (or `None` when the outcome is not worth a message).
///
/// A failed send re-queues ten minutes out and counts an attempt; the entry is
/// only given up on once [`MAX_ATTEMPTS`] sends have failed, so a connection
/// that is momentarily busy does not silently end the promise.
pub(crate) fn apply_outcome(
    entry: &mut QuotaResumeEntry,
    outcome: ResumeOutcome,
    now: DateTime<Utc>,
) -> Option<String> {
    match outcome {
        ResumeOutcome::Sent => {
            entry.status = ResumeStatus::Done;
            Some(resumed_notice(entry))
        }
        ResumeOutcome::ConnectionGone => {
            entry.status = ResumeStatus::ConnectionGone;
            Some(connection_gone_notice(entry))
        }
        ResumeOutcome::SendFailed => {
            entry.attempts = entry.attempts.saturating_add(1);
            if entry.attempts >= MAX_ATTEMPTS {
                entry.status = ResumeStatus::Failed;
                Some(failed_notice(entry))
            } else {
                // Measured from `now`, not from the stale `resume_at`: the entry
                // is by definition already due, so adding to it would schedule
                // the retry in the past and burn every remaining attempt in as
                // many ticks.
                entry.resume_at = now + RETRY_DELAY;
                None
            }
        }
    }
}

/// The notice sent when the conversation was resumed.
pub fn resumed_notice(entry: &QuotaResumeEntry) -> String {
    format!(
        "▶️ Reanudé la conversación #{} ({}) — se recuperó la cuota.",
        entry.conversation_id, entry.agent_type
    )
}

/// The notice sent when the connection was no longer open.
pub fn connection_gone_notice(entry: &QuotaResumeEntry) -> String {
    format!(
        "⚠️ No pude reanudar la conversación #{}: la sesión ya no está abierta en Phantom.",
        entry.conversation_id
    )
}

/// The notice sent once every attempt has failed.
pub fn failed_notice(entry: &QuotaResumeEntry) -> String {
    format!(
        "⚠️ No pude reanudar la conversación #{} ({}) tras {} intentos. \
         Ábrela de nuevo y pulsa «Esperar y continuar solo».",
        entry.conversation_id, entry.agent_type, entry.attempts
    )
}

/// Insert `entry`, replacing any pending schedule for the same conversation.
///
/// Rescheduling is the common case — the user presses *Esperar* again after
/// changing their mind — and two pending entries for one conversation would
/// fire two "continue where you left off" prompts into the same agent turn.
/// Finished entries for that conversation are left alone as history.
pub(crate) fn upsert_pending(entries: &mut Vec<QuotaResumeEntry>, entry: QuotaResumeEntry) {
    entries.retain(|e| !(e.status.is_pending() && e.conversation_id == entry.conversation_id));
    entries.push(entry);
    prune_entries(entries);
}

/// Cap the registry at [`MAX_ENTRIES`]. Finished entries go first, oldest
/// first; only if every entry is still pending does the oldest pending one go,
/// so the cap is a hard bound rather than a best effort.
pub(crate) fn prune_entries(entries: &mut Vec<QuotaResumeEntry>) {
    while entries.len() > MAX_ENTRIES {
        let victim = entries
            .iter()
            .position(|e| !e.status.is_pending())
            .unwrap_or(0);
        entries.remove(victim);
    }
}

/// Project the registry into the API shape, pending first and soonest first —
/// the order the UI wants to render in, computed here so both runtimes agree.
pub(crate) fn scheduled_entries_view(entries: Vec<QuotaResumeEntry>) -> Vec<QuotaResumeView> {
    let mut rows: Vec<QuotaResumeView> = entries
        .into_iter()
        .map(|e| QuotaResumeView {
            id: e.id,
            conversation_id: e.conversation_id,
            agent_type: e.agent_type,
            resume_at: e.resume_at,
            status: e.status,
            attempts: e.attempts,
        })
        .collect();
    rows.sort_by(|a, b| {
        b.status
            .is_pending()
            .cmp(&a.status.is_pending())
            .then_with(|| a.resume_at.cmp(&b.resume_at))
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

// ─── Persistence ────────────────────────────────────────────────────────

async fn load_entries(conn: &DatabaseConnection) -> Vec<QuotaResumeEntry> {
    match app_metadata_service::get_value(conn, KEY).await {
        Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
        // A row that cannot be parsed is treated as an empty registry: the
        // alternative is a permanently broken "Esperar" button.
        _ => Vec::new(),
    }
}

async fn save_entries(
    conn: &DatabaseConnection,
    entries: &[QuotaResumeEntry],
) -> Result<(), AppCommandError> {
    let json = serde_json::to_string(entries)
        .map_err(|e| AppCommandError::database_error(format!("quota resume encode: {e}")))?;
    app_metadata_service::upsert_value(conn, KEY, &json)
        .await
        .map_err(AppCommandError::from)
}

// ─── Effects seam ───────────────────────────────────────────────────────

/// Everything [`process_due`] needs from the outside world. Two
/// implementations exist: [`ConnectionManagerEffects`], the real one, and a
/// test fake — so the loop's own logic is exercised without a network, a
/// subprocess, or a Telegram message.
#[async_trait]
pub(crate) trait ResumeEffects: Send + Sync {
    /// Whether `connection_id` is still an open ACP session.
    async fn is_live(&self, connection_id: &str) -> bool;
    /// Hand [`RESUME_PROMPT`] to the connection. `Err` means the send failed.
    async fn send_resume(&self, connection_id: &str) -> Result<(), String>;
    /// Tell the user over Telegram.
    async fn notify(&self, text: &str);
}

/// The production effects: the live [`ConnectionManager`] plus the `nexus
/// notify` CLI, which owns the bot token and must not be reimplemented here.
pub(crate) struct ConnectionManagerEffects {
    manager: ConnectionManager,
}

impl ConnectionManagerEffects {
    pub(crate) fn new(manager: ConnectionManager) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl ResumeEffects for ConnectionManagerEffects {
    async fn is_live(&self, connection_id: &str) -> bool {
        let Some(state) = self.manager.get_state(connection_id).await else {
            return false;
        };
        let status = state.read().await.status.clone();
        !matches!(
            status,
            ConnectionStatus::Disconnected | ConnectionStatus::Error
        )
    }

    async fn send_resume(&self, connection_id: &str) -> Result<(), String> {
        self.manager
            .send_prompt(
                connection_id,
                vec![PromptInputBlock::Text {
                    text: RESUME_PROMPT.to_string(),
                }],
            )
            .await
            .map_err(|e| e.to_string())
    }

    async fn notify(&self, text: &str) {
        nexus_notify(text).await;
    }
}

/// Send one Telegram message through the NEXUS CLI. Fire-and-forget: a notice
/// that fails to go out must never turn a successful resume into a failure, so
/// every outcome here is logged and dropped.
async fn nexus_notify(text: &str) {
    const TIMEOUT: Duration = Duration::from_secs(15);
    let mut cmd = tokio::process::Command::new(crate::chat_channel::agenda_tap::nexus_bin());
    cmd.arg("notify")
        .arg(text)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Ok(Ok(out)) if out.status.success() => {}
        Ok(Ok(out)) => tracing::warn!(
            status = %out.status,
            "[quota_resume] nexus notify exited unsuccessfully"
        ),
        Ok(Err(e)) => tracing::warn!(error = %e, "[quota_resume] could not run nexus notify"),
        Err(_) => tracing::warn!("[quota_resume] nexus notify timed out"),
    }
}

// ─── Shell ──────────────────────────────────────────────────────────────

/// Schedule a resume for `conversation_id`, replacing any pending schedule for
/// the same conversation. Returns the new entry's id and fire time.
pub async fn schedule_core(
    conn: &DatabaseConnection,
    connection_id: String,
    conversation_id: i32,
    agent_type: AgentType,
) -> Result<QuotaResumeScheduled, AppCommandError> {
    let now = Utc::now();
    let wire = agent_type.as_wire().into_owned();

    // Claude's live quota first: it is the only source that knows when the
    // *subscription* frees up. Cached for 60 s, and absent when the read
    // failed — in which case the recorded limit hit is the fallback.
    let claude = crate::commands::usage_meter::claude_usage_cached().await;
    let limits = model_limits::load_all(conn).await;
    let limit_resets_at = limits.get(&wire).and_then(|l| l.resets_at);

    let entry = QuotaResumeEntry {
        id: new_id(),
        connection_id,
        conversation_id,
        agent_type: wire.clone(),
        resume_at: pick_resume_at(now, &wire, claude.as_ref(), limit_resets_at),
        status: ResumeStatus::Pending,
        attempts: 0,
        created_at: now,
    };

    let mut entries = load_entries(conn).await;
    upsert_pending(&mut entries, entry.clone());
    save_entries(conn, &entries).await?;

    tracing::info!(
        conversation_id,
        agent = %wire,
        resume_at = %entry.resume_at,
        "[quota_resume] scheduled an automatic resume"
    );
    Ok(QuotaResumeScheduled {
        id: entry.id,
        resume_at: entry.resume_at,
    })
}

/// Every schedule, pending first and soonest first.
pub async fn list_core(conn: &DatabaseConnection) -> Result<Vec<QuotaResumeView>, AppCommandError> {
    Ok(scheduled_entries_view(load_entries(conn).await))
}

/// Cancel a pending schedule. Cancelling an unknown id, or one that already
/// finished, is not an error: the user's intent ("do not resume this") holds
/// either way, and the UI should not have to distinguish the two.
pub async fn cancel_core(
    conn: &DatabaseConnection,
    id: &str,
) -> Result<QuotaResumeCanceled, AppCommandError> {
    let mut entries = load_entries(conn).await;
    if let Some(entry) = entries
        .iter_mut()
        .find(|e| e.id == id && e.status.is_pending())
    {
        entry.status = ResumeStatus::Canceled;
        save_entries(conn, &entries).await?;
        tracing::info!(id = %id, "[quota_resume] canceled a scheduled resume");
    }
    Ok(QuotaResumeCanceled { ok: true })
}

/// One pass of the background loop: resume everything due at `now`.
///
/// Each entry is settled independently and the registry is written once at the
/// end, so one bad connection cannot roll back another entry's outcome. Notices
/// go out after the write: telling the user "I resumed it" before the fact is
/// durably recorded would be a lie if the process died in between.
pub(crate) async fn process_due(
    conn: &DatabaseConnection,
    effects: &dyn ResumeEffects,
    now: DateTime<Utc>,
) -> Result<usize, AppCommandError> {
    let mut entries = load_entries(conn).await;
    let due = due_indices(&entries, now);
    if due.is_empty() {
        return Ok(0);
    }
    let processed = due.len();

    let mut notices: Vec<String> = Vec::new();
    for idx in due {
        let (connection_id, conversation_id, id) = {
            let entry = &entries[idx];
            (
                entry.connection_id.clone(),
                entry.conversation_id,
                entry.id.clone(),
            )
        };

        let outcome = if !effects.is_live(&connection_id).await {
            ResumeOutcome::ConnectionGone
        } else {
            match effects.send_resume(&connection_id).await {
                Ok(()) => ResumeOutcome::Sent,
                Err(err) => {
                    tracing::warn!(
                        id = %id,
                        conversation_id,
                        error = %err,
                        "[quota_resume] could not send the resume prompt"
                    );
                    ResumeOutcome::SendFailed
                }
            }
        };

        if let Some(text) = apply_outcome(&mut entries[idx], outcome, now) {
            notices.push(text);
        }
    }

    prune_entries(&mut entries);
    save_entries(conn, &entries).await?;
    for text in notices {
        effects.notify(&text).await;
    }
    Ok(processed)
}

/// The background task. One per process, started at startup by both runtimes;
/// [`spawn_resume_loop`] is the idempotent entry point.
pub async fn run_resume_loop(conn: DatabaseConnection, manager: ConnectionManager) {
    let effects = ConnectionManagerEffects::new(manager);
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        if let Err(err) = process_due(&conn, &effects, Utc::now()).await {
            tracing::warn!(error = %err, "[quota_resume] tick failed");
        }
    }
}

/// Start [`run_resume_loop`] once per process.
///
/// Idempotent through a `OnceLock` rather than a caller's care: both runtimes
/// call it from their setup path, and a second loop would double every resume.
pub fn spawn_resume_loop(conn: DatabaseConnection, manager: ConnectionManager) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    tokio::spawn(run_resume_loop(conn, manager));
}

// ─── Desktop commands ───────────────────────────────────────────────────

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn quota_resume_schedule(
    connection_id: String,
    conversation_id: i32,
    agent_type: AgentType,
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<QuotaResumeScheduled, AppCommandError> {
    schedule_core(&db.conn, connection_id, conversation_id, agent_type).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn quota_resume_list(
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<Vec<QuotaResumeView>, AppCommandError> {
    list_core(&db.conn).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn quota_resume_cancel(
    id: String,
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<QuotaResumeCanceled, AppCommandError> {
    cancel_core(&db.conn, &id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::sync::Mutex;

    use crate::models::usage_meter::{ClaudeUsage, ClaudeUsageWindow};

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid ts")
            .with_timezone(&Utc)
    }

    fn claude(five_pct: f64, five_at: &str, week_pct: f64, week_at: &str) -> ClaudeUsage {
        ClaudeUsage {
            plan: "pro".to_string(),
            five_hour: ClaudeUsageWindow {
                percent: five_pct,
                resets_at: Some(five_at.to_string()),
            },
            seven_day: ClaudeUsageWindow {
                percent: week_pct,
                resets_at: Some(week_at.to_string()),
            },
            fetched_at: ts("2026-10-10T09:00:00Z").to_rfc3339(),
        }
    }

    fn entry(
        id: &str,
        conversation_id: i32,
        status: ResumeStatus,
        resume_at: &str,
    ) -> QuotaResumeEntry {
        QuotaResumeEntry {
            id: id.to_string(),
            connection_id: "conn-1".to_string(),
            conversation_id,
            agent_type: "claude_code".to_string(),
            resume_at: ts(resume_at),
            status,
            attempts: 0,
            created_at: ts("2026-10-10T09:00:00Z"),
        }
    }

    // ─── pick_resume_at ───────────────────────────────────────────────

    #[test]
    fn a_spent_five_hour_window_decides_when_claude_comes_back() {
        let now = ts("2026-10-10T09:00:00Z");
        let usage = claude(99.5, "2026-10-10T11:00:00Z", 60.0, "2026-10-14T12:00:00Z");
        let at = pick_resume_at(now, "claude_code", Some(&usage), None);
        // The five-hour reset, plus the buffer.
        assert_eq!(at, ts("2026-10-10T11:01:00Z"));
    }

    #[test]
    fn an_unspent_five_hour_window_falls_through_to_the_week() {
        let now = ts("2026-10-10T09:00:00Z");
        let usage = claude(40.0, "2026-10-10T11:00:00Z", 99.1, "2026-10-14T12:00:00Z");
        assert_eq!(
            pick_resume_at(now, "claude_code", Some(&usage), None),
            ts("2026-10-14T12:01:00Z")
        );
    }

    #[test]
    fn a_half_spent_quota_is_not_a_limit_so_the_recorded_hit_decides() {
        // Neither window is saturated: Claude's numbers say nothing about when
        // the *limit* lifts, so the limit-hit row is the only honest source.
        let now = ts("2026-10-10T09:00:00Z");
        let usage = claude(40.0, "2026-10-10T11:00:00Z", 60.0, "2026-10-14T12:00:00Z");
        let limit = ts("2026-10-10T15:30:00Z");
        assert_eq!(
            pick_resume_at(now, "claude_code", Some(&usage), Some(limit)),
            ts("2026-10-10T15:31:00Z")
        );
    }

    #[test]
    fn with_no_information_at_all_it_still_promises_a_resume() {
        let now = ts("2026-10-10T09:00:00Z");
        assert_eq!(
            pick_resume_at(now, "claude_code", None, None),
            now + FALLBACK_DELAY + RESET_BUFFER
        );
    }

    #[test]
    fn a_saturated_window_without_a_reset_time_falls_back_to_the_limit_row() {
        // Percent alone is not enough to schedule on — resuming at the wrong
        // instant burns an attempt and re-prompts into a live limit.
        let now = ts("2026-10-10T09:00:00Z");
        let usage = claude(100.0, "", 100.0, "not-a-timestamp");
        let limit = ts("2026-10-10T15:30:00Z");
        assert_eq!(
            pick_resume_at(now, "claude_code", Some(&usage), Some(limit)),
            ts("2026-10-10T15:31:00Z")
        );
    }

    #[test]
    fn another_agents_quota_never_decides_its_schedule() {
        // An OpenCode limit must not be scheduled at Claude's reset time.
        let now = ts("2026-10-10T09:00:00Z");
        let usage = claude(100.0, "2026-10-10T11:00:00Z", 100.0, "2026-10-14T12:00:00Z");
        let limit = ts("2026-10-10T15:30:00Z");
        assert_eq!(
            pick_resume_at(now, "open_code", Some(&usage), Some(limit)),
            ts("2026-10-10T15:31:00Z")
        );
    }

    // ─── due_indices ──────────────────────────────────────────────────

    #[test]
    fn only_pending_entries_at_or_before_now_are_due() {
        let now = ts("2026-10-10T10:00:00Z");
        let entries = vec![
            entry("a", 1, ResumeStatus::Pending, "2026-10-10T09:59:00Z"),
            entry("b", 2, ResumeStatus::Pending, "2026-10-10T10:00:00Z"),
            entry("c", 3, ResumeStatus::Pending, "2026-10-10T10:00:01Z"),
            entry("d", 4, ResumeStatus::Done, "2026-10-10T09:00:00Z"),
            entry("e", 5, ResumeStatus::ConnectionGone, "2026-10-10T09:00:00Z"),
        ];
        // Exactly at `now` counts as due: waiting for the next tick would just
        // add up to 30 s of silence for no reason.
        assert_eq!(due_indices(&entries, now), vec![0, 1]);
    }

    #[test]
    fn an_empty_registry_has_nothing_due() {
        assert!(due_indices(&[], ts("2026-10-10T10:00:00Z")).is_empty());
    }

    // ─── apply_outcome ────────────────────────────────────────────────

    #[test]
    fn a_sent_resume_finishes_the_entry_and_says_so() {
        let mut e = entry("a", 7, ResumeStatus::Pending, "2026-10-10T10:00:00Z");
        let notice = apply_outcome(&mut e, ResumeOutcome::Sent, ts("2026-10-10T10:00:00Z"));
        assert_eq!(e.status, ResumeStatus::Done);
        assert_eq!(e.attempts, 0);
        assert_eq!(
            notice.as_deref(),
            Some("▶️ Reanudé la conversación #7 (claude_code) — se recuperó la cuota.")
        );
    }

    #[test]
    fn a_vanished_connection_ends_the_entry_without_sending_anything() {
        let mut e = entry("a", 7, ResumeStatus::Pending, "2026-10-10T10:00:00Z");
        let notice = apply_outcome(
            &mut e,
            ResumeOutcome::ConnectionGone,
            ts("2026-10-10T10:00:00Z"),
        );
        assert_eq!(e.status, ResumeStatus::ConnectionGone);
        assert_eq!(e.attempts, 0, "no send was attempted");
        assert_eq!(
            notice.as_deref(),
            Some(
                "⚠️ No pude reanudar la conversación #7: la sesión ya no está abierta en Phantom."
            )
        );
    }

    #[test]
    fn a_failed_send_waits_ten_minutes_and_tries_again_quietly() {
        let now = ts("2026-10-10T10:00:00Z");
        let mut e = entry("a", 7, ResumeStatus::Pending, "2026-10-10T10:00:00Z");
        let notice = apply_outcome(&mut e, ResumeOutcome::SendFailed, now);
        assert_eq!(e.status, ResumeStatus::Pending, "still trying");
        assert_eq!(e.attempts, 1);
        assert_eq!(e.resume_at, now + RETRY_DELAY);
        assert!(notice.is_none(), "a retry is not news");
    }

    #[test]
    fn the_third_failed_send_gives_up_and_says_why() {
        let now = ts("2026-10-10T10:00:00Z");
        let mut e = entry("a", 7, ResumeStatus::Pending, "2026-10-10T10:00:00Z");
        e.attempts = MAX_ATTEMPTS - 1;
        let notice = apply_outcome(&mut e, ResumeOutcome::SendFailed, now);
        assert_eq!(e.status, ResumeStatus::Failed);
        assert_eq!(e.attempts, MAX_ATTEMPTS);
        let notice = notice.expect("the last failure is worth a message");
        assert!(notice.contains("#7"), "{notice}");
        assert!(notice.contains("3 intentos"), "{notice}");
    }

    // ─── upsert_pending / prune_entries ───────────────────────────────

    #[test]
    fn rescheduling_replaces_the_pending_entry_for_that_conversation() {
        let mut entries = vec![
            entry("old", 7, ResumeStatus::Pending, "2026-10-10T15:00:00Z"),
            entry("other", 8, ResumeStatus::Pending, "2026-10-10T16:00:00Z"),
        ];
        let fresh = entry("new", 7, ResumeStatus::Pending, "2026-10-10T20:00:00Z");
        upsert_pending(&mut entries, fresh);

        let for_seven: Vec<&QuotaResumeEntry> =
            entries.iter().filter(|e| e.conversation_id == 7).collect();
        assert_eq!(for_seven.len(), 1, "one prompt per conversation");
        assert_eq!(for_seven[0].id, "new");
        assert!(entries.iter().any(|e| e.id == "other"), "unrelated kept");
    }

    #[test]
    fn a_finished_entry_survives_a_reschedule_as_history() {
        let mut entries = vec![entry("done", 7, ResumeStatus::Done, "2026-10-10T15:00:00Z")];
        upsert_pending(
            &mut entries,
            entry("new", 7, ResumeStatus::Pending, "2026-10-10T20:00:00Z"),
        );
        assert_eq!(entries.len(), 2);
    }

    #[test]
    fn the_registry_is_capped_by_dropping_the_oldest_finished_first() {
        let mut entries: Vec<QuotaResumeEntry> = (0..MAX_ENTRIES)
            .map(|i| {
                entry(
                    &format!("f{i}"),
                    i as i32,
                    ResumeStatus::Done,
                    "2026-10-10T10:00:00Z",
                )
            })
            .collect();
        upsert_pending(
            &mut entries,
            entry("fresh", 999, ResumeStatus::Pending, "2026-10-10T20:00:00Z"),
        );

        assert_eq!(entries.len(), MAX_ENTRIES);
        assert!(
            entries.iter().any(|e| e.id == "fresh"),
            "the new entry is kept"
        );
        assert!(
            !entries.iter().any(|e| e.id == "f0"),
            "the oldest is dropped"
        );
        assert!(entries.iter().any(|e| e.id == "f1"), "only one was dropped");
    }

    #[test]
    fn a_registry_of_only_pending_entries_is_still_capped() {
        let mut entries: Vec<QuotaResumeEntry> = (0..MAX_ENTRIES + 5)
            .map(|i| {
                entry(
                    &format!("p{i}"),
                    i as i32,
                    ResumeStatus::Pending,
                    "2026-10-10T20:00:00Z",
                )
            })
            .collect();
        prune_entries(&mut entries);
        assert_eq!(entries.len(), MAX_ENTRIES);
    }

    // ─── scheduled_entries_view ───────────────────────────────────────

    #[test]
    fn the_list_puts_pending_work_first_and_soonest_first() {
        let rows = scheduled_entries_view(vec![
            entry("late", 1, ResumeStatus::Pending, "2026-10-10T18:00:00Z"),
            entry("finished", 2, ResumeStatus::Done, "2026-10-10T09:00:00Z"),
            entry("soon", 3, ResumeStatus::Pending, "2026-10-10T11:00:00Z"),
        ]);
        let ids: Vec<&str> = rows.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, vec!["soon", "late", "finished"]);
    }

    // ─── persistence round-trip (no network, no ACP, no notify) ───────

    #[tokio::test]
    async fn the_registry_survives_a_reload_through_the_metadata_row() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let entry = entry("a", 7, ResumeStatus::Pending, "2026-10-10T15:00:00Z");
        let mut entries = vec![entry.clone()];
        save_entries(&db.conn, &entries).await.expect("save");

        assert_eq!(load_entries(&db.conn).await, vec![entry.clone()]);

        entries[0].status = ResumeStatus::Done;
        entries[0].attempts = 2;
        save_entries(&db.conn, &entries).await.expect("save again");
        let reloaded = load_entries(&db.conn).await;
        assert_eq!(reloaded[0].status, ResumeStatus::Done);
        assert_eq!(reloaded[0].attempts, 2);
        assert_eq!(reloaded[0].resume_at, entry.resume_at);
    }

    #[tokio::test]
    async fn an_unreadable_row_reads_as_an_empty_registry_instead_of_an_error() {
        // A corrupt row must not wedge the "Esperar" button forever.
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        app_metadata_service::upsert_value(&db.conn, KEY, "{ not json")
            .await
            .expect("seed");
        assert!(load_entries(&db.conn).await.is_empty());
    }

    #[tokio::test]
    async fn a_missing_row_reads_as_an_empty_registry() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        assert!(load_entries(&db.conn).await.is_empty());
    }

    #[tokio::test]
    async fn cancelling_marks_the_entry_and_leaves_the_rest_alone() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        save_entries(
            &db.conn,
            &[
                entry("a", 7, ResumeStatus::Pending, "2026-10-10T15:00:00Z"),
                entry("b", 8, ResumeStatus::Pending, "2026-10-10T16:00:00Z"),
            ],
        )
        .await
        .expect("save");

        let res = cancel_core(&db.conn, "a").await.expect("cancel");
        assert!(res.ok);

        let rows = list_core(&db.conn).await.expect("list");
        let a = rows.iter().find(|r| r.id == "a").expect("a");
        assert_eq!(a.status, ResumeStatus::Canceled);
        assert_eq!(
            rows.iter().find(|r| r.id == "b").unwrap().status,
            ResumeStatus::Pending
        );
    }

    #[tokio::test]
    async fn cancelling_something_already_finished_is_still_a_yes() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        save_entries(
            &db.conn,
            &[entry("a", 7, ResumeStatus::Done, "2026-10-10T15:00:00Z")],
        )
        .await
        .expect("save");

        assert!(cancel_core(&db.conn, "a").await.expect("cancel").ok);
        assert!(
            cancel_core(&db.conn, "never-existed")
                .await
                .expect("cancel")
                .ok
        );

        let rows = list_core(&db.conn).await.expect("list");
        assert_eq!(
            rows[0].status,
            ResumeStatus::Done,
            "history is not rewritten"
        );
    }

    // ─── the loop, over a fake transport ──────────────────────────────

    /// Records what the loop asked for; stands in for both the ACP connection
    /// and the Telegram bot. The trait takes `&self`, so the recordings live
    /// behind a mutex — the fake mirrors the real effects' shared, mutable
    /// process-wide side rather than pretending it owns exclusive state.
    #[derive(Default)]
    struct FakeEffects {
        live: Vec<String>,
        fail: bool,
        sent: Mutex<Vec<String>>,
        notices: Mutex<Vec<String>>,
    }

    impl FakeEffects {
        fn new(live: &[&str], fail: bool) -> Self {
            Self {
                live: live.iter().map(|s| s.to_string()).collect(),
                fail,
                sent: Mutex::new(Vec::new()),
                notices: Mutex::new(Vec::new()),
            }
        }

        fn sent(&self) -> Vec<String> {
            self.sent.lock().expect("fake mutex").clone()
        }

        fn notices(&self) -> Vec<String> {
            self.notices.lock().expect("fake mutex").clone()
        }
    }

    #[async_trait]
    impl ResumeEffects for FakeEffects {
        async fn is_live(&self, connection_id: &str) -> bool {
            self.live.iter().any(|c| c == connection_id)
        }

        async fn send_resume(&self, connection_id: &str) -> Result<(), String> {
            if self.fail {
                return Err("connection busy".to_string());
            }
            self.sent
                .lock()
                .expect("fake mutex")
                .push(connection_id.to_string());
            Ok(())
        }

        async fn notify(&self, text: &str) {
            self.notices
                .lock()
                .expect("fake mutex")
                .push(text.to_string());
        }
    }

    #[tokio::test]
    async fn the_loop_resumes_a_due_entry_and_announces_it() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        save_entries(
            &db.conn,
            &[
                entry("due", 7, ResumeStatus::Pending, "2026-10-10T10:00:00Z"),
                entry("later", 8, ResumeStatus::Pending, "2026-10-10T11:00:00Z"),
            ],
        )
        .await
        .expect("save");

        let effects = FakeEffects::new(&["conn-1"], false);
        let processed = process_due(&db.conn, &effects, ts("2026-10-10T10:00:00Z"))
            .await
            .expect("tick");

        assert_eq!(processed, 1);
        assert_eq!(effects.sent(), vec!["conn-1"]);
        assert_eq!(effects.notices().len(), 1);
        assert!(effects.notices()[0].contains("Reanudé la conversación #7"));

        let rows = list_core(&db.conn).await.expect("list");
        let due = rows.iter().find(|r| r.id == "due").unwrap();
        assert_eq!(due.status, ResumeStatus::Done);
        assert_eq!(
            rows.iter().find(|r| r.id == "later").unwrap().status,
            ResumeStatus::Pending
        );
    }

    #[tokio::test]
    async fn the_loop_reports_a_vanished_connection_instead_of_failing_silently() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        save_entries(
            &db.conn,
            &[entry(
                "due",
                7,
                ResumeStatus::Pending,
                "2026-10-10T10:00:00Z",
            )],
        )
        .await
        .expect("save");

        let effects = FakeEffects::new(&[], false);
        process_due(&db.conn, &effects, ts("2026-10-10T10:00:00Z"))
            .await
            .expect("tick");

        assert!(effects.sent().is_empty(), "nothing to send to");
        assert_eq!(effects.notices().len(), 1);
        assert!(effects.notices()[0].contains("ya no está abierta"));

        let rows = list_core(&db.conn).await.expect("list");
        assert_eq!(rows[0].status, ResumeStatus::ConnectionGone);
    }

    #[tokio::test]
    async fn a_failed_send_is_retried_then_given_up_on() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        save_entries(
            &db.conn,
            &[entry(
                "due",
                7,
                ResumeStatus::Pending,
                "2026-10-10T10:00:00Z",
            )],
        )
        .await
        .expect("save");

        let effects = FakeEffects::new(&["conn-1"], true);

        // First tick, on the instant it came due: attempt 1, retry at 10:10.
        process_due(&db.conn, &effects, ts("2026-10-10T10:00:00Z"))
            .await
            .expect("tick");
        let rows = list_core(&db.conn).await.expect("list");
        assert_eq!(rows[0].attempts, 1);
        assert_eq!(rows[0].status, ResumeStatus::Pending);
        assert_eq!(rows[0].resume_at, ts("2026-10-10T10:10:00Z"));
        assert!(effects.notices().is_empty(), "a retry is not news");

        // Before the retry comes due, ticks do nothing at all.
        process_due(&db.conn, &effects, ts("2026-10-10T10:05:00Z"))
            .await
            .expect("tick");
        assert_eq!(list_core(&db.conn).await.unwrap()[0].attempts, 1);

        // Then the second attempt, and ten minutes later the third.
        process_due(&db.conn, &effects, ts("2026-10-10T10:10:00Z"))
            .await
            .expect("tick");
        assert_eq!(list_core(&db.conn).await.unwrap()[0].attempts, 2);
        process_due(&db.conn, &effects, ts("2026-10-10T10:20:00Z"))
            .await
            .expect("tick");

        let rows = list_core(&db.conn).await.expect("list");
        assert_eq!(rows[0].status, ResumeStatus::Failed);
        assert_eq!(rows[0].attempts, MAX_ATTEMPTS);
        assert_eq!(effects.notices().len(), 1, "only the give-up is news");
        assert!(effects.notices()[0].contains("3 intentos"));

        // And it never fires again.
        assert_eq!(
            process_due(&db.conn, &effects, ts("2026-10-11T10:00:00Z"))
                .await
                .expect("tick"),
            0
        );
    }

    #[tokio::test]
    async fn a_tick_with_nothing_due_touches_nothing() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        save_entries(
            &db.conn,
            &[entry(
                "due",
                7,
                ResumeStatus::Pending,
                "2026-10-10T10:00:00Z",
            )],
        )
        .await
        .expect("save");

        let effects = FakeEffects::new(&["conn-1"], false);
        assert_eq!(
            process_due(&db.conn, &effects, ts("2026-10-10T09:59:00Z"))
                .await
                .expect("tick"),
            0
        );
        assert!(effects.notices().is_empty());
        assert_eq!(
            list_core(&db.conn).await.unwrap()[0].status,
            ResumeStatus::Pending
        );
    }

    #[test]
    fn ids_are_unique_and_prefixed() {
        let a = new_id();
        let b = new_id();
        assert_ne!(a, b);
        assert!(a.starts_with("qr-"), "{a}");
        assert_eq!(a.len(), "qr-".len() + 12);
    }
}
