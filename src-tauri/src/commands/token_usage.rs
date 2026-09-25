//! Token-usage dashboard: materialize per-turn spend, then aggregate it.
//!
//! ## Why a materialized table at all
//!
//! Token counts live only inside each agent's own transcript on disk. Reading
//! one conversation's numbers means parsing one file, which is what the
//! conversation panel already does. A dashboard needs *every* conversation at
//! once, so it reads a materialized copy instead: [`token_usage_sync_core`]
//! parses each conversation once and writes one row per usage-bearing turn;
//! everything the dashboard shows is folded from those rows.
//!
//! ## What "incremental" covers, and what it does not
//!
//! A conversation is re-parsed when its `conversation.updated_at` differs from
//! what the last sync recorded. That covers:
//!
//!   * every codeg-driven turn — each one flips the row's status, bumping
//!     `updated_at`;
//!   * every freshly imported session — it has no stamp at all;
//!   * every **re-imported** session — `import_one` drops the stamp explicitly,
//!     because a re-import deliberately does not bump `updated_at` and is
//!     precisely the moment a CLI-grown transcript comes back into view.
//!
//! One case it does NOT cover: a transcript that grows in the agent's own CLI
//! and is then never re-imported and never touched inside codeg. Nothing in the
//! database moves, so nothing marks it stale. Closing that automatically would
//! need a per-conversation transcript fingerprint the parser layer does not
//! expose (each agent stores history differently — single file, many files, or
//! its own SQLite). [`TokenUsageSyncMode::Full`] is the answer, and the UI
//! surfaces it as "Rebuild all" with exactly that explanation.
//!
//! ## Why the fold happens in Rust, not SQL
//!
//! Every bucket the dashboard draws — calendar day, ISO week, month, hour of
//! day, weekday — is a *local-time* bucket, and the local zone belongs to the
//! viewer, not the database. Expressing that in SQLite means string-formatting
//! shifted timestamps and hoping the stored text format cooperates. Instead the
//! SQL layer does what it is good at (an indexed range scan plus the dimension
//! filters) and [`aggregate_report`] folds the surviving rows in one pass — a
//! pure function over a `Vec`, which is both exactly testable and free of
//! date-format landmines.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Timelike, Utc};
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};

use crate::app_error::AppCommandError;
use crate::commands::conversations::get_folder_conversation_core;
use crate::db::entities::folder;
use crate::db::service::app_metadata_service;
use crate::db::service::token_usage_service::{
    self as usage_service, FactQuery, UsageFact, UsageFactRow,
};
use crate::models::conversation::DbConversationDetail;
use crate::models::message::{ContentBlock, MessageTurn};
use crate::models::token_usage::{
    TokenUsageBreakdownItem, TokenUsageBucket, TokenUsageConversationItem, TokenUsageFacets,
    TokenUsageFilter, TokenUsageFolderFacet, TokenUsageHeatCell, TokenUsagePoint,
    TokenUsageReport, TokenUsageStreak, TokenUsageSyncProgress, TokenUsageSyncResult,
    TokenUsageSyncStatus, TokenUsageTotals,
};
use crate::web::event_bridge::{emit_event, EventEmitter, TOKEN_USAGE_SYNC_PROGRESS_EVENT};

use sea_orm::EntityTrait;

/// Hard ceiling on fact rows pulled into memory for one report. At ~90 bytes a
/// row this is well under 50 MB, and it is roughly a decade of continuous heavy
/// use — a range that reaches it is pathological, not routine. Rows are read
/// newest-first, so hitting the cap keeps the recent history the dashboard is
/// mostly about and the report is flagged `truncated`.
pub const MAX_SCANNED_FACTS: u64 = 500_000;

/// Above this many buckets the series stops being densified (empty buckets are
/// dropped instead of emitted as zeros). Only reachable by asking for daily
/// buckets across several years of history.
const MAX_DENSE_BUCKETS: usize = 3_000;

/// How many transcripts are parsed concurrently during a sync. Parsing is
/// CPU+IO on the blocking pool; the DB writes stay sequential behind it, since
/// SQLite serializes writers anyway.
const PARSE_CONCURRENCY: usize = 4;

/// Minimum gap between progress events, so a 5 000-conversation sync doesn't
/// push 5 000 messages through the event bus.
const PROGRESS_INTERVAL_MS: i64 = 150;

/// How many conversations the "biggest sessions" list returns.
const TOP_CONVERSATIONS: usize = 8;

/// Identifies the accounting the stored facts were produced under. Bump it
/// whenever a change makes previously written rows wrong rather than merely
/// stale — the next sync then rebuilds everything instead of trusting stamps
/// that only ever tracked whether the *transcript* moved.
///
/// Without this, a fix to how tokens are counted would reach only the
/// conversations that happen to be touched afterwards, and the dashboard would
/// keep serving a mix of old wrong numbers and new right ones indefinitely.
///
/// * `1` — the original per-turn accounting.
/// * `2` — one usage record per API call (Claude wrote one line per content
///   block, each repeating the same usage); every Codex model round-trip
///   counted via cumulative-counter deltas; Claude `Task` sub-agent transcripts
///   counted against the session that launched them; and facts anchored at the
///   turn's own timestamp instead of its last tool result.
/// * `3` — Qoder's cached prefix counted once instead of twice. Its transcripts
///   carry Anthropic FIELD NAMES over OpenAI SEMANTICS, so `input_tokens` is
///   the whole prompt and `cache_read_input_tokens` is a subset of it; summing
///   the two (which every shared helper does, because for Claude they are
///   disjoint) inflated input and total by the cached amount. See
///   `parsers::qoder::qoder_turn_usage`.
/// * `4` — added the twelve tool-quality counters (`tool_calls`/`tool_errors`
///   and the five category pairs — see `tool_counters_from_blocks`). Every row
///   written under `1`–`3` has them at their column default (`0`), which reads
///   as "no tool calls recorded" rather than "recorded and empty" — wrong for
///   any conversation that actually used tools. Bumping the version is what
///   gets real counts into rows that already exist; a fresh row from a
///   conversation with genuinely no tool calls (a short chat) is unaffected
///   either way.
///
/// Only the accounting stored in `token_usage_turn` counts: the four token
/// counters, the duration, the timestamp, and (from schema `4`) the
/// tool-quality counters. A conversation's context WINDOW is not stored here
/// (nor anywhere else — `SessionStats` is recomputed by the parser on every
/// read), so changing how a window is inferred needs no bump; it reaches every
/// existing session the moment it ships.
///
/// Nor does a change to the DATA a transcript carries: shipping Qoder's
/// `QODER_EXPOSE_TOKEN_USAGE` launch env only affects turns recorded after it,
/// which are new rows either way. `3` is here because the same release also
/// changed how an UNCHANGED transcript is read — and Qoder sessions with real
/// counters predate it, since a custom/BYO model has always exposed them and
/// the parser reads every session under `~/.qoder/projects`, not just the ones
/// codeg launched.
const FACT_SCHEMA_VERSION: &str = "4";

const FACT_SCHEMA_VERSION_KEY: &str = "token_usage_fact_schema_version";

/// Serializes syncs. `try_lock` rather than queueing: a second sync racing the
/// first would re-parse the same files for no benefit, and the caller can just
/// be told one is already running.
static SYNC_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Set while [`token_usage_sync_core`] is inside the guard, so
/// [`token_usage_status_core`] can report "a sync is running" without trying to
/// take the lock (which would itself be a race).
static SYNC_RUNNING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// What a sync pass is allowed to skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TokenUsageSyncMode {
    /// Re-parse only conversations whose row moved since their last parse.
    #[default]
    Incremental,
    /// Drop everything and re-parse every conversation. The escape hatch for a
    /// transcript that grew without codeg noticing.
    Full,
}

// ─── Fact extraction ────────────────────────────────────────────────────

/// u64 → i64 for storage. Token counters never approach `i64::MAX`; the clamp
/// exists so a corrupt transcript reporting a nonsense counter can't wrap into
/// a negative row that would silently subtract from every total.
fn clamp_i64(v: u64) -> i64 {
    v.min(i64::MAX as u64) as i64
}

/// Turn a parsed conversation's turns into storable facts.
///
/// Only turns that actually report usage are kept — a zero-token turn would add
/// a row that contributes nothing but inflates the turn count.
///
/// `occurred_at` is the turn's **own** timestamp — when the assistant spoke and
/// the usage was reported — not `completed_at`. The two differ by more than
/// they look: a turn absorbs the tool results that follow it, so `completed_at`
/// is the moment the *last tool* finished, and one stalled tool (a long build,
/// a command left waiting overnight) drags it arbitrarily far from the API call
/// that actually spent the tokens. Real transcripts hold gaps of more than a
/// day, which is exactly how a turn's spend lands on a calendar day it had
/// nothing to do with. The turn's own timestamp is bounded by the turn itself,
/// and it keeps the series in transcript order.
///
/// `session_model` backfills turns whose parser records no per-turn model.
/// Codex is the motivating case: its rollout files carry the model in
/// `turn_context` events, which the parser folds into the *session* summary
/// while every turn stays `model: None` — without the fallback, all of a Codex
/// session's spend lands in the dashboard's "unknown model" slice. A turn that
/// does name its own model always wins, so mid-session switches recorded by a
/// parser are never overwritten.
pub(crate) fn facts_from_turns(
    turns: &[MessageTurn],
    session_model: Option<&str>,
) -> Vec<UsageFact> {
    let session_model = session_model.map(str::trim).filter(|m| !m.is_empty());
    turns
        .iter()
        .enumerate()
        .filter_map(|(idx, turn)| {
            let usage = turn.usage.as_ref()?;
            let input = clamp_i64(usage.input_tokens);
            let output = clamp_i64(usage.output_tokens);
            let cache_create = clamp_i64(usage.cache_creation_input_tokens);
            let cache_read = clamp_i64(usage.cache_read_input_tokens);
            if input == 0 && output == 0 && cache_create == 0 && cache_read == 0 {
                return None;
            }
            let turn_key = if turn.id.is_empty() {
                format!("turn-{idx}")
            } else {
                turn.id.clone()
            };
            let model = turn
                .model
                .as_ref()
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty())
                .or_else(|| session_model.map(String::from));
            let counters = tool_counters_from_blocks(&turn.blocks);
            Some(UsageFact {
                turn_key,
                occurred_at: turn.timestamp,
                model,
                input_tokens: input,
                output_tokens: output,
                cache_creation_tokens: cache_create,
                cache_read_tokens: cache_read,
                duration_ms: clamp_i64(turn.duration_ms.unwrap_or(0)),
                tool_calls: counters.tool_calls,
                tool_errors: counters.tool_errors,
                edit_calls: counters.edit_calls,
                edit_errors: counters.edit_errors,
                read_calls: counters.read_calls,
                read_errors: counters.read_errors,
                shell_calls: counters.shell_calls,
                shell_errors: counters.shell_errors,
                web_calls: counters.web_calls,
                web_errors: counters.web_errors,
                agent_calls: counters.agent_calls,
                agent_errors: counters.agent_errors,
            })
        })
        .collect()
}

/// Which quality-tracked category a tool belongs to. A tool that matches none
/// still counts toward the turn's overall `tool_calls`/`tool_errors`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ToolCategory {
    Edit,
    Read,
    Shell,
    Web,
    Agent,
}

/// Tokens (some with a single `*` wildcard) that classify a normalized
/// (lowercased) tool name into a category. Checked in this exact order —
/// `Web` before `Read` matters: `websearch` and a hypothetical
/// `web_search`-style id both contain `search` (a `Read` token), so `Web`'s
/// more specific compound tokens must win first. Every other pair of lists is
/// collision-free for the tool names agents actually emit.
const EDIT_TOKENS: &[&str] = &[
    "edit",
    "write",
    "multiedit",
    "apply_patch",
    "str_replace*",
    "notebookedit",
    "patch",
];
const SHELL_TOKENS: &[&str] = &["bash", "shell", "execute", "terminal", "run*command"];
const WEB_TOKENS: &[&str] = &["webfetch", "websearch", "fetch", "browse*", "web_*"];
const READ_TOKENS: &[&str] = &[
    "read",
    "grep",
    "glob",
    "ls",
    "list",
    "search",
    "find",
    "codesearch",
];
const AGENT_TOKENS: &[&str] = &["agent", "task", "subagent", "delegate_to_agent"];
/// Planning / meta tools whose names collide with the tokens above (`TodoWrite`
/// contains `write`, `ToolSearch` contains `search`) but say nothing about a
/// model's editing, reading or shell reliability. Matched exactly, first.
const UNCATEGORIZED_TOOLS: &[&str] = &[
    "todowrite",
    "todo_write",
    "todoread",
    "todo_read",
    "toolsearch",
    "enterplanmode",
    "exitplanmode",
    "askuserquestion",
    "skill",
];

/// Does `name` match a glob-ish `pattern` containing at most one `*`
/// wildcard? `*` at the end means "starts with the prefix"; `*` in the middle
/// means "contains both halves, in order"; no `*` means a plain substring
/// test. Good enough for the short, hand-written token lists above — not a
/// general glob engine.
fn glob_contains(name: &str, pattern: &str) -> bool {
    match pattern.split_once('*') {
        None => name.contains(pattern),
        Some((before, "")) => name.contains(before),
        Some((before, after)) => match name.find(before) {
            Some(pos) => name[pos + before.len()..].contains(after),
            None => false,
        },
    }
}

fn matches_any(name: &str, tokens: &[&str]) -> bool {
    tokens.iter().any(|t| glob_contains(name, t))
}

/// Classify a tool's name (case-insensitive) into a quality-tracked category,
/// or `None` when it matches none of them — such a call still counts toward
/// the turn's overall `tool_calls`/`tool_errors`.
pub(crate) fn categorize_tool(tool_name: &str) -> Option<ToolCategory> {
    let name = tool_name.trim().to_lowercase();
    if name.is_empty() || UNCATEGORIZED_TOOLS.contains(&name.as_str()) {
        return None;
    }
    if matches_any(&name, EDIT_TOKENS) {
        Some(ToolCategory::Edit)
    } else if matches_any(&name, WEB_TOKENS) {
        Some(ToolCategory::Web)
    } else if matches_any(&name, SHELL_TOKENS) {
        Some(ToolCategory::Shell)
    } else if matches_any(&name, READ_TOKENS) {
        Some(ToolCategory::Read)
    } else if matches_any(&name, AGENT_TOKENS) {
        Some(ToolCategory::Agent)
    } else {
        None
    }
}

/// Tool-quality counters for one turn, folded from its content blocks.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ToolCounters {
    pub tool_calls: i64,
    pub tool_errors: i64,
    pub edit_calls: i64,
    pub edit_errors: i64,
    pub read_calls: i64,
    pub read_errors: i64,
    pub shell_calls: i64,
    pub shell_errors: i64,
    pub web_calls: i64,
    pub web_errors: i64,
    pub agent_calls: i64,
    pub agent_errors: i64,
}

impl ToolCounters {
    fn bump_call(&mut self, category: Option<ToolCategory>) {
        self.tool_calls += 1;
        match category {
            Some(ToolCategory::Edit) => self.edit_calls += 1,
            Some(ToolCategory::Read) => self.read_calls += 1,
            Some(ToolCategory::Shell) => self.shell_calls += 1,
            Some(ToolCategory::Web) => self.web_calls += 1,
            Some(ToolCategory::Agent) => self.agent_calls += 1,
            None => {}
        }
    }

    fn bump_error(&mut self, category: Option<ToolCategory>) {
        self.tool_errors += 1;
        match category {
            Some(ToolCategory::Edit) => self.edit_errors += 1,
            Some(ToolCategory::Read) => self.read_errors += 1,
            Some(ToolCategory::Shell) => self.shell_errors += 1,
            Some(ToolCategory::Web) => self.web_errors += 1,
            Some(ToolCategory::Agent) => self.agent_errors += 1,
            None => {}
        }
    }
}

/// Count tool calls and tool errors in one turn's content blocks.
///
/// A call is a `ToolUse` block — counted immediately, whether or not a result
/// ever shows up. An error is a `ToolResult` block with `is_error: true`,
/// matched back to its call by `tool_use_id` so it is attributed to the right
/// tool name and category; a result with no matching call (shouldn't happen,
/// but a malformed transcript is not this function's problem) is silently
/// skipped rather than guessed at.
pub(crate) fn tool_counters_from_blocks(blocks: &[ContentBlock]) -> ToolCounters {
    let mut names_by_id: HashMap<&str, &str> = HashMap::new();
    let mut counters = ToolCounters::default();

    for block in blocks {
        if let ContentBlock::ToolUse {
            tool_use_id,
            tool_name,
            ..
        } = block
        {
            if let Some(id) = tool_use_id.as_deref() {
                names_by_id.insert(id, tool_name.as_str());
            }
            counters.bump_call(categorize_tool(tool_name));
        }
    }

    for block in blocks {
        if let ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } = block
        {
            if !*is_error {
                continue;
            }
            let Some(name) = tool_use_id.as_deref().and_then(|id| names_by_id.get(id)) else {
                continue;
            };
            counters.bump_error(categorize_tool(name));
        }
    }

    counters
}

/// `turn_key` of the whole-session fallback row. Distinct enough from any
/// parser's turn id to be recognizable when auditing the table.
pub(crate) const SESSION_TOTAL_TURN_KEY: &str = "__session_total__";

/// Every fact a parsed conversation contributes.
///
/// Normally that is one row per usage-bearing turn. Some agents, though, only
/// know their token counts at the session level: Hermes reads cumulative
/// `input/output/cache` columns off its own SQLite `sessions` row and leaves
/// every message's `usage` as `None`, precisely because its per-message rows
/// carry no input/output split. Counting only per-turn usage would report those
/// conversations as **zero tokens** — a confident wrong number, not a missing
/// one.
///
/// So when a conversation yields no per-turn facts but does carry a session
/// total, that total is written as a single row. The trade-off is explicit: the
/// session's whole spend lands in one bucket (its last turn's) instead of being
/// spread across the turns that produced it, which is right for every total and
/// breakdown and coarse only on the time axis. The fallback cannot double-count
/// — it fires only when the per-turn pass produced nothing at all.
///
/// Agents that record no token data anywhere (Cursor) still contribute nothing,
/// which is the honest answer for them.
pub(crate) fn facts_from_detail(
    detail: &DbConversationDetail,
    fallback_at: DateTime<Utc>,
) -> Vec<UsageFact> {
    let session_model = detail.summary.model.as_deref();
    let per_turn = facts_from_turns(&detail.turns, session_model);
    if !per_turn.is_empty() {
        return per_turn;
    }

    let Some(stats) = detail.session_stats.as_ref() else {
        return Vec::new();
    };
    let Some(usage) = stats.total_usage.as_ref() else {
        return Vec::new();
    };
    let input = clamp_i64(usage.input_tokens);
    let output = clamp_i64(usage.output_tokens);
    let cache_create = clamp_i64(usage.cache_creation_input_tokens);
    let cache_read = clamp_i64(usage.cache_read_input_tokens);
    if input == 0 && output == 0 && cache_create == 0 && cache_read == 0 {
        return Vec::new();
    }

    // The end of the conversation is the least-wrong single instant for a
    // session-wide total; `fallback_at` (the row's `updated_at`) covers a
    // conversation whose transcript has stats but no turns at all.
    let occurred_at = detail
        .turns
        .last()
        .map(|t| t.completed_at.unwrap_or(t.timestamp))
        .unwrap_or(fallback_at);

    vec![UsageFact {
        turn_key: SESSION_TOTAL_TURN_KEY.to_string(),
        occurred_at,
        model: detail
            .summary
            .model
            .as_ref()
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty()),
        input_tokens: input,
        output_tokens: output,
        cache_creation_tokens: cache_create,
        cache_read_tokens: cache_read,
        duration_ms: clamp_i64(stats.total_duration_ms),
        // No per-turn blocks to count for a whole-session fallback row — the
        // agent never reported per-turn usage in the first place, so there is
        // nothing to attribute tool calls to at finer granularity than "the
        // session as a whole", which this row already is.
        ..Default::default()
    }]
}

// ─── Local-time bucket math ─────────────────────────────────────────────

fn local_naive(dt: DateTime<Utc>, offset_minutes: i32) -> NaiveDateTime {
    dt.naive_utc() + Duration::minutes(offset_minutes as i64)
}

fn utc_from_local_naive(n: NaiveDateTime, offset_minutes: i32) -> DateTime<Utc> {
    DateTime::<Utc>::from_naive_utc_and_offset(n - Duration::minutes(offset_minutes as i64), Utc)
}

/// The first local calendar day of the bucket `date` falls in.
fn bucket_start_date(date: NaiveDate, bucket: TokenUsageBucket) -> NaiveDate {
    match bucket {
        TokenUsageBucket::Day => date,
        TokenUsageBucket::Week => {
            date - Duration::days(date.weekday().num_days_from_monday() as i64)
        }
        // `with_day(1)` on a valid date always yields a valid date.
        TokenUsageBucket::Month => date.with_day(1).unwrap_or(date),
    }
}

/// The bucket immediately after the one starting at `date`.
fn next_bucket_date(date: NaiveDate, bucket: TokenUsageBucket) -> NaiveDate {
    match bucket {
        TokenUsageBucket::Day => date + Duration::days(1),
        TokenUsageBucket::Week => date + Duration::days(7),
        TokenUsageBucket::Month => {
            let (y, m) = if date.month() == 12 {
                (date.year() + 1, 1)
            } else {
                (date.year(), date.month() + 1)
            };
            NaiveDate::from_ymd_opt(y, m, 1).unwrap_or(date + Duration::days(31))
        }
    }
}

fn bucket_key(date: NaiveDate, bucket: TokenUsageBucket) -> String {
    match bucket {
        TokenUsageBucket::Month => format!("{:04}-{:02}", date.year(), date.month()),
        _ => date.format("%Y-%m-%d").to_string(),
    }
}

// ─── Aggregation ────────────────────────────────────────────────────────

/// Running sums for one group (a bucket, or one slice of a breakdown).
#[derive(Debug, Default, Clone)]
struct Acc {
    input: u64,
    output: u64,
    cache_create: u64,
    cache_read: u64,
    total: u64,
    duration_ms: u64,
    turns: u64,
    conversations: HashSet<i32>,
}

impl Acc {
    fn add(&mut self, row: &UsageFactRow) {
        // Stored counters are non-negative by construction (`clamp_i64` on a
        // u64), but read them defensively: a hand-edited DB must not be able to
        // produce a negative total that reads as "less than nothing".
        self.input += row.input_tokens.max(0) as u64;
        self.output += row.output_tokens.max(0) as u64;
        self.cache_create += row.cache_creation_tokens.max(0) as u64;
        self.cache_read += row.cache_read_tokens.max(0) as u64;
        self.total += row.total_tokens.max(0) as u64;
        self.duration_ms += row.duration_ms.max(0) as u64;
        self.turns += 1;
        self.conversations.insert(row.conversation_id);
    }

    fn into_totals(self, active_days: u32) -> TokenUsageTotals {
        TokenUsageTotals {
            input_tokens: self.input,
            output_tokens: self.output,
            cache_creation_tokens: self.cache_create,
            cache_read_tokens: self.cache_read,
            total_tokens: self.total,
            turn_count: self.turns,
            conversation_count: self.conversations.len() as u64,
            duration_ms: self.duration_ms,
            active_days,
        }
    }

    fn into_breakdown(self, key: String, label: String) -> TokenUsageBreakdownItem {
        TokenUsageBreakdownItem {
            key,
            label,
            input_tokens: self.input,
            output_tokens: self.output,
            cache_creation_tokens: self.cache_create,
            cache_read_tokens: self.cache_read,
            total_tokens: self.total,
            turn_count: self.turns,
            conversation_count: self.conversations.len() as u64,
        }
    }
}

/// Everything [`aggregate_report`] needs that isn't a fact row.
pub(crate) struct AggregateOptions<'a> {
    pub bucket: TokenUsageBucket,
    pub tz_offset_minutes: i32,
    /// Explicit lower bound from the filter; `None` clamps to the first fact.
    pub range_start: Option<DateTime<Utc>>,
    /// Explicit, exclusive upper bound; `None` clamps to just past the last fact.
    pub range_end: Option<DateTime<Utc>>,
    pub folder_labels: &'a HashMap<i32, String>,
    pub truncated: bool,
    /// A comparison window was requested. Kept separate from "are there
    /// previous rows": a window that was asked for and came back empty is a
    /// real answer (the UI renders it as "new"), and collapsing it to `None`
    /// would make a first-ever period look like one with no comparison at all.
    pub compare_previous: bool,
}

/// Fold filtered facts into everything one dashboard render needs.
///
/// One pass over `rows` feeds every output at once — buckets, the three
/// breakdowns, the weekday×hour grid, the per-conversation tally and the active
/// day set — because a second pass would just re-walk the same vector.
pub(crate) fn aggregate_report(
    rows: &[UsageFactRow],
    previous_rows: &[UsageFactRow],
    opts: &AggregateOptions<'_>,
) -> TokenUsageReport {
    let tz = opts.tz_offset_minutes;
    let bucket = opts.bucket;

    let mut totals = Acc::default();
    let mut by_bucket: HashMap<NaiveDate, Acc> = HashMap::new();
    let mut by_folder: HashMap<i32, Acc> = HashMap::new();
    let mut by_agent: HashMap<String, Acc> = HashMap::new();
    let mut by_model: HashMap<String, Acc> = HashMap::new();
    let mut heat: HashMap<(u8, u8), (u64, u64)> = HashMap::new();
    let mut per_conversation: HashMap<i32, (u64, u64, DateTime<Utc>, String, i32)> = HashMap::new();
    let mut active_dates: HashSet<NaiveDate> = HashSet::new();
    let mut first_activity: Option<DateTime<Utc>> = None;
    let mut last_activity: Option<DateTime<Utc>> = None;

    for row in rows {
        let local = local_naive(row.occurred_at, tz);
        let date = local.date();

        totals.add(row);
        by_bucket
            .entry(bucket_start_date(date, bucket))
            .or_default()
            .add(row);
        by_folder.entry(row.folder_id).or_default().add(row);
        by_agent
            .entry(row.agent_type.clone())
            .or_default()
            .add(row);
        // A turn whose transcript never named a model still spent tokens, so it
        // is counted under an explicit "unknown" slice rather than dropped.
        by_model
            .entry(row.model.clone().unwrap_or_else(|| UNKNOWN_MODEL.to_string()))
            .or_default()
            .add(row);

        let cell = heat
            .entry((
                local.weekday().num_days_from_monday() as u8,
                local.hour() as u8,
            ))
            .or_insert((0, 0));
        cell.0 += row.total_tokens.max(0) as u64;
        cell.1 += 1;

        let entry = per_conversation.entry(row.conversation_id).or_insert((
            0,
            0,
            row.occurred_at,
            row.agent_type.clone(),
            row.folder_id,
        ));
        entry.0 += row.total_tokens.max(0) as u64;
        entry.1 += 1;
        if row.occurred_at > entry.2 {
            entry.2 = row.occurred_at;
        }

        active_dates.insert(date);
        if first_activity.is_none_or(|f| row.occurred_at < f) {
            first_activity = Some(row.occurred_at);
        }
        if last_activity.is_none_or(|l| row.occurred_at > l) {
            last_activity = Some(row.occurred_at);
        }
    }

    let active_days = active_dates.len() as u32;
    let series = build_series(&by_bucket, opts, first_activity, last_activity);

    let mut folder_items: Vec<TokenUsageBreakdownItem> = by_folder
        .into_iter()
        .map(|(id, acc)| {
            let label = opts
                .folder_labels
                .get(&id)
                .cloned()
                .unwrap_or_else(|| format!("#{id}"));
            acc.into_breakdown(id.to_string(), label)
        })
        .collect();
    sort_breakdown(&mut folder_items);

    let mut agent_items: Vec<TokenUsageBreakdownItem> = by_agent
        .into_iter()
        .map(|(key, acc)| {
            let label = key.clone();
            acc.into_breakdown(key, label)
        })
        .collect();
    sort_breakdown(&mut agent_items);

    let mut model_items: Vec<TokenUsageBreakdownItem> = by_model
        .into_iter()
        .map(|(key, acc)| {
            let label = key.clone();
            acc.into_breakdown(key, label)
        })
        .collect();
    sort_breakdown(&mut model_items);

    let mut heatmap: Vec<TokenUsageHeatCell> = heat
        .into_iter()
        .map(|((weekday, hour), (total_tokens, turn_count))| TokenUsageHeatCell {
            weekday,
            hour,
            total_tokens,
            turn_count,
        })
        .collect();
    heatmap.sort_by_key(|a| (a.weekday, a.hour));

    let mut top: Vec<(i32, u64, u64, DateTime<Utc>, String, i32)> = per_conversation
        .into_iter()
        .map(|(id, (tokens, turns, last, agent, folder_id))| {
            (id, tokens, turns, last, agent, folder_id)
        })
        .collect();
    // Ties broken by id so the list is stable across identical requests.
    top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    top.truncate(TOP_CONVERSATIONS);
    let top_conversations: Vec<TokenUsageConversationItem> = top
        .into_iter()
        .map(
            |(id, tokens, turns, last, agent, folder_id)| TokenUsageConversationItem {
                conversation_id: id,
                // Filled by the command layer, which owns the DB handle.
                title: None,
                agent_type: agent,
                folder_label: opts.folder_labels.get(&folder_id).cloned(),
                total_tokens: tokens,
                turn_count: turns,
                last_activity_at: last,
            },
        )
        .collect();

    let previous_totals = opts.compare_previous.then(|| {
        let mut prev = Acc::default();
        let mut prev_dates: HashSet<NaiveDate> = HashSet::new();
        for row in previous_rows {
            prev.add(row);
            prev_dates.insert(local_naive(row.occurred_at, tz).date());
        }
        prev.into_totals(prev_dates.len() as u32)
    });

    TokenUsageReport {
        range_start: opts.range_start.or(first_activity),
        // Always exclusive, so a consumer that prints "the last day counted"
        // can subtract one instant unconditionally. When the filter had no
        // upper bound this is one tick past the last recorded turn.
        range_end: opts
            .range_end
            .or_else(|| last_activity.map(|t| t + Duration::milliseconds(1))),
        bucket,
        totals: totals.into_totals(active_days),
        previous_totals,
        series,
        by_folder: folder_items,
        by_agent: agent_items,
        by_model: model_items,
        heatmap,
        top_conversations,
        streak: compute_streak(&active_dates),
        first_activity_at: first_activity,
        last_activity_at: last_activity,
        truncated: opts.truncated,
    }
}

/// Model slice for turns whose transcript never named one. A sentinel rather
/// than a dropped row, so the by-model chart's slices still sum to the total.
pub(crate) const UNKNOWN_MODEL: &str = "__unknown__";

fn sort_breakdown(items: &mut [TokenUsageBreakdownItem]) {
    // Tokens descending; the key breaks ties so repeated requests agree.
    items.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens).then(a.key.cmp(&b.key)));
}

/// Emit one point per bucket between the range bounds, including buckets with
/// no activity, so the chart's x-axis carries real gaps instead of silently
/// closing them up.
fn build_series(
    by_bucket: &HashMap<NaiveDate, Acc>,
    opts: &AggregateOptions<'_>,
    first_activity: Option<DateTime<Utc>>,
    last_activity: Option<DateTime<Utc>>,
) -> Vec<TokenUsagePoint> {
    let tz = opts.tz_offset_minutes;
    let bucket = opts.bucket;

    let point_of = |date: NaiveDate, acc: Option<&Acc>| {
        let next = next_bucket_date(date, bucket);
        let acc = acc.cloned().unwrap_or_default();
        TokenUsagePoint {
            bucket_key: bucket_key(date, bucket),
            start: utc_from_local_naive(date.and_hms_opt(0, 0, 0).unwrap_or_default(), tz),
            end: utc_from_local_naive(next.and_hms_opt(0, 0, 0).unwrap_or_default(), tz),
            input_tokens: acc.input,
            output_tokens: acc.output,
            cache_creation_tokens: acc.cache_create,
            cache_read_tokens: acc.cache_read,
            total_tokens: acc.total,
            turn_count: acc.turns,
            conversation_count: acc.conversations.len() as u64,
        }
    };

    let sparse = |by_bucket: &HashMap<NaiveDate, Acc>| {
        let mut dates: Vec<NaiveDate> = by_bucket.keys().copied().collect();
        dates.sort_unstable();
        dates
            .into_iter()
            .map(|d| point_of(d, by_bucket.get(&d)))
            .collect::<Vec<_>>()
    };

    // With no explicit bounds the densified span is the data's own extent; an
    // explicit filter range wins so a deliberately empty window still renders
    // its full axis (that emptiness is the answer).
    let (Some(lo), Some(hi_inclusive)) = (
        opts.range_start.or(first_activity),
        // `range_end` is exclusive: step back an instant so a range ending
        // exactly on a bucket boundary doesn't render a trailing empty bucket.
        opts.range_end
            .map(|e| e - Duration::milliseconds(1))
            .or(last_activity),
    ) else {
        return sparse(by_bucket);
    };

    let first = bucket_start_date(local_naive(lo, tz).date(), bucket);
    let last = bucket_start_date(local_naive(hi_inclusive, tz).date(), bucket);
    if last < first {
        return sparse(by_bucket);
    }

    let mut out: Vec<TokenUsagePoint> = Vec::new();
    let mut cursor = first;
    while cursor <= last {
        out.push(point_of(cursor, by_bucket.get(&cursor)));
        if out.len() >= MAX_DENSE_BUCKETS {
            // Pathological span (daily buckets over many years). Fall back to
            // the sparse form rather than returning a truncated axis that would
            // silently hide the tail.
            return sparse(by_bucket);
        }
        cursor = next_bucket_date(cursor, bucket);
    }
    out
}

/// Longest and trailing runs of consecutive active local days.
fn compute_streak(active: &HashSet<NaiveDate>) -> TokenUsageStreak {
    if active.is_empty() {
        return TokenUsageStreak::default();
    }
    let mut dates: Vec<NaiveDate> = active.iter().copied().collect();
    dates.sort_unstable();

    let mut longest = 1u32;
    let mut run = 1u32;
    for pair in dates.windows(2) {
        if pair[1] == pair[0] + Duration::days(1) {
            run += 1;
        } else {
            run = 1;
        }
        longest = longest.max(run);
    }
    TokenUsageStreak {
        longest_days: longest,
        // `run` ends at the last date by construction.
        current_days: run,
        current_ends_on: dates.last().map(|d| d.format("%Y-%m-%d").to_string()),
    }
}

// ─── Report command ─────────────────────────────────────────────────────

async fn folder_label_map(
    conn: &sea_orm::DatabaseConnection,
) -> Result<HashMap<i32, String>, AppCommandError> {
    let folders = folder::Entity::find()
        .all(conn)
        .await
        .map_err(|e| AppCommandError::from(crate::db::error::DbError::from(e)))?;
    Ok(folders
        .into_iter()
        .map(|f| (f.id, usage_service::folder_display_label(&f)))
        .collect())
}

/// Translate the wire filter into a storage query, expanding each selected
/// folder to the worktree folders parented to it.
async fn to_fact_query(
    conn: &sea_orm::DatabaseConnection,
    filter: &TokenUsageFilter,
) -> Result<FactQuery, AppCommandError> {
    let folder_ids = match filter.folder_ids {
        // An explicitly empty selection is preserved (it means "nothing"), so
        // `FactQuery::is_empty_selection` can short-circuit it.
        Some(ref ids) if ids.is_empty() => Some(Vec::new()),
        Some(ref ids) => Some(
            usage_service::expand_folder_ids(conn, ids)
                .await
                .map_err(AppCommandError::from)?,
        ),
        None => None,
    };
    Ok(FactQuery {
        start: filter.start,
        end: filter.end,
        folder_ids,
        agent_types: filter.agent_types.clone(),
        models: filter.models.clone(),
    })
}

pub async fn token_usage_report_core(
    conn: &sea_orm::DatabaseConnection,
    filter: TokenUsageFilter,
) -> Result<TokenUsageReport, AppCommandError> {
    let query = to_fact_query(conn, &filter).await?;
    let rows = usage_service::fetch_facts(conn, &query, MAX_SCANNED_FACTS)
        .await
        .map_err(AppCommandError::from)?;
    let mut truncated = rows.len() as u64 >= MAX_SCANNED_FACTS;

    // The comparison window is the same span immediately before the range, and
    // is read through the same path so both sides are computed identically.
    // It needs both bounds: an open-ended range has no "same span before it".
    let comparable = matches!(
        (filter.compare_previous, filter.start, filter.end),
        (true, Some(start), Some(end)) if end > start
    );
    let previous_rows = match (comparable, filter.start, filter.end) {
        (true, Some(start), Some(end)) => {
            let span = end - start;
            let prev_query = FactQuery {
                start: Some(start - span),
                end: Some(start),
                ..query.clone()
            };
            let prev = usage_service::fetch_facts(conn, &prev_query, MAX_SCANNED_FACTS)
                .await
                .map_err(AppCommandError::from)?;
            // A capped comparison window makes the delta chips wrong too, so it
            // flags the whole report — the notice is about "these numbers are
            // partial", which is equally true of a partial baseline.
            truncated |= prev.len() as u64 >= MAX_SCANNED_FACTS;
            prev
        }
        _ => Vec::new(),
    };

    let folder_labels = folder_label_map(conn).await?;
    let opts = AggregateOptions {
        bucket: filter.bucket,
        tz_offset_minutes: normalize_tz_offset(filter.tz_offset_minutes),
        range_start: filter.start,
        range_end: filter.end,
        folder_labels: &folder_labels,
        truncated,
        compare_previous: comparable,
    };
    let mut report = aggregate_report(&rows, &previous_rows, &opts);

    // The headline session count follows the workspace list, not the fact
    // table. The fold's distinct-id count includes delegation children the
    // sidebar hides and misses sessions that never recorded usage (empty ones,
    // agents without token counts, transcripts the agent's retention already
    // deleted) — so with an unbounded range it would visibly disagree with the
    // status bar's session counter. The strip's averages divide by the same
    // number, so the cell stays self-consistent.
    report.totals.conversation_count =
        usage_service::workspace_conversation_count(conn, &query).await?;

    // Titles are the one thing the fold can't produce — resolve just the ids
    // that made the top list.
    let top_ids: Vec<i32> = report
        .top_conversations
        .iter()
        .map(|c| c.conversation_id)
        .collect();
    let labels = usage_service::conversation_labels(conn, &top_ids)
        .await
        .map_err(AppCommandError::from)?;
    for item in report.top_conversations.iter_mut() {
        if let Some((title, _, _)) = labels.get(&item.conversation_id) {
            item.title = title.clone();
        }
    }

    Ok(report)
}

/// Clamp a client-supplied UTC offset to the range real zones occupy
/// (UTC-12:00 … UTC+14:00). Guards the bucket math against a nonsense value
/// shifting every timestamp into a different era.
fn normalize_tz_offset(minutes: i32) -> i32 {
    minutes.clamp(-12 * 60, 14 * 60)
}

pub async fn token_usage_facets_core(
    conn: &sea_orm::DatabaseConnection,
) -> Result<TokenUsageFacets, AppCommandError> {
    let folders = usage_service::facet_folders(conn)
        .await
        .map_err(AppCommandError::from)?;
    let agents = usage_service::facet_agents(conn)
        .await
        .map_err(AppCommandError::from)?;
    let models = usage_service::facet_models(conn)
        .await
        .map_err(AppCommandError::from)?;
    let (data_start, data_end) = usage_service::facet_extent(conn)
        .await
        .map_err(AppCommandError::from)?;

    Ok(TokenUsageFacets {
        folders: folders
            .into_iter()
            .map(|(f, _)| TokenUsageFolderFacet {
                folder_id: f.id,
                label: usage_service::folder_display_label(&f),
                name: f.name.clone(),
                alias: f
                    .alias
                    .as_ref()
                    .map(|a| a.trim())
                    .filter(|a| !a.is_empty())
                    .map(str::to_owned),
                path: f.path.clone(),
                parent_id: f.parent_id,
            })
            .collect(),
        agents,
        models,
        data_start,
        data_end,
    })
}

pub async fn token_usage_status_core(
    conn: &sea_orm::DatabaseConnection,
) -> Result<TokenUsageSyncStatus, AppCommandError> {
    let counts = usage_service::sync_counts(conn)
        .await
        .map_err(AppCommandError::from)?;
    Ok(TokenUsageSyncStatus {
        total_conversations: counts.total_conversations,
        synced_conversations: counts.synced_conversations,
        stale_conversations: counts.stale_conversations,
        fact_rows: counts.fact_rows,
        last_synced_at: counts.last_synced_at,
        running: SYNC_RUNNING.load(std::sync::atomic::Ordering::Relaxed),
    })
}

// ─── Sync ───────────────────────────────────────────────────────────────

/// True when a parse came back empty for a conversation that previously had
/// recorded usage — the signature of an unreadable source rather than a real
/// drop to zero.
///
/// The test is `turns.is_empty()`, not `facts.is_empty()`: a conversation whose
/// turns parsed fine but genuinely carry no token counts (Cursor) must still be
/// stamped, or every pass would re-parse it forever. Only a transcript that
/// produced *no turns at all* while we hold recorded facts for it is treated as
/// lost.
fn parse_lost_a_readable_source(
    detail: &DbConversationDetail,
    candidate: &usage_service::SyncCandidate,
) -> bool {
    detail.turns.is_empty()
        && detail.session_stats.is_none()
        && candidate.synced_turn_count.unwrap_or(0) > 0
}

/// Whether this pass re-parses a given conversation.
///
/// `Full` re-parses everything, but note what it does NOT do: it never wipes
/// the tables up front. Each conversation's rows are swapped inside its own
/// transaction, so a rebuild has no window where the dashboard reads an empty
/// table, and — more importantly — [`parse_lost_a_readable_source`] still has
/// the previous turn counts to reason about. A wipe-first rebuild would delete
/// the facts of a conversation whose transcript happens to be unreachable right
/// now and then stamp it as counted, losing that history for good. "Rebuild"
/// means re-derive everything derivable, not destroy whatever cannot be
/// re-derived today.
fn should_reparse(candidate: &usage_service::SyncCandidate, mode: TokenUsageSyncMode) -> bool {
    mode == TokenUsageSyncMode::Full || candidate.is_stale()
}

/// Clears [`SYNC_RUNNING`] however the sync ends — early return, `?`, or panic.
struct RunningFlag;

impl Drop for RunningFlag {
    fn drop(&mut self) {
        SYNC_RUNNING.store(false, std::sync::atomic::Ordering::Relaxed);
    }
}

pub async fn token_usage_sync_core(
    conn: &sea_orm::DatabaseConnection,
    emitter: &EventEmitter,
    mode: TokenUsageSyncMode,
) -> Result<TokenUsageSyncResult, AppCommandError> {
    let _guard = SYNC_GUARD
        .try_lock()
        .map_err(|_| AppCommandError::invalid_input("A token usage sync is already running"))?;
    SYNC_RUNNING.store(true, std::sync::atomic::Ordering::Relaxed);
    let _running = RunningFlag;

    // Rows written under older accounting are wrong, not stale, so no stamp
    // will ever mark them for re-parse. Upgrading the mode is what actually
    // delivers a counting fix to history the user already has.
    let recorded_schema =
        app_metadata_service::get_value(conn, FACT_SCHEMA_VERSION_KEY)
            .await
            .map_err(AppCommandError::from)?;
    let schema_changed = recorded_schema.as_deref() != Some(FACT_SCHEMA_VERSION);
    let mode = if schema_changed {
        tracing::info!(
            from = recorded_schema.as_deref().unwrap_or("<none>"),
            to = FACT_SCHEMA_VERSION,
            "token usage sync: fact accounting changed, rebuilding every conversation"
        );
        TokenUsageSyncMode::Full
    } else {
        mode
    };

    let mut result = TokenUsageSyncResult {
        pruned_conversations: usage_service::prune_orphaned_facts(conn)
            .await
            .map_err(AppCommandError::from)?,
        ..Default::default()
    };

    let candidates = usage_service::list_sync_candidates(conn)
        .await
        .map_err(AppCommandError::from)?;
    result.scanned = candidates.len() as u32;

    let stale: Vec<_> = candidates
        .into_iter()
        .filter(|c| should_reparse(c, mode))
        .collect();
    result.skipped = result.scanned - stale.len() as u32;

    let total = stale.len() as u32;
    if total == 0 {
        if schema_changed {
            record_fact_schema_version(conn).await?;
        }
        emit_event(
            emitter,
            TOKEN_USAGE_SYNC_PROGRESS_EVENT,
            TokenUsageSyncProgress {
                done: 0,
                total: 0,
                current_title: None,
                result: Some(result.clone()),
            },
        );
        return Ok(result);
    }

    let mut done = 0u32;
    let mut last_emit = Utc::now() - Duration::milliseconds(PROGRESS_INTERVAL_MS);

    // Parse concurrently (each `get_folder_conversation_core` hands the file
    // read to the blocking pool), consume in order, write sequentially.
    let mut parsed = stream::iter(stale.into_iter().map(|candidate| async move {
        let detail = get_folder_conversation_core(conn, candidate.id).await;
        (candidate, detail)
    }))
    .buffered(PARSE_CONCURRENCY);

    while let Some((candidate, detail)) = parsed.next().await {
        done += 1;
        match detail {
            Ok((detail, _)) if parse_lost_a_readable_source(&detail, &candidate) => {
                // `get_folder_conversation_core` reports a transcript it can no
                // longer find as a *successful* parse with zero turns (see its
                // `ConversationNotFound` arm). Writing that through would delete
                // real recorded usage, so the fact rows are left alone.
                //
                // The stamp, though, is settled rather than held open. There is
                // no third state to wait for: a transcript the agent's CLI
                // deleted is not coming back, and nothing here can tell a first
                // miss from a thousandth. Holding the conversation stale means
                // re-walking the agent's whole transcript tree on every pass,
                // failing identically each time, and reporting it — a loop with
                // no exit that re-derives nothing. So keep the numbers, close
                // the case, and say it at `debug` because a settled source is a
                // resting state, not an incident. A re-import or a full rebuild
                // is what puts the conversation back in scope.
                tracing::debug!(
                    conversation_id = candidate.id,
                    kept_turns = candidate.synced_turn_count.unwrap_or(0),
                    "token usage sync: transcript is gone — keeping the recorded \
                     facts and settling the stamp"
                );
                match usage_service::settle_lost_source(conn, candidate.id, candidate.updated_at)
                    .await
                {
                    Ok(()) => result.lost += 1,
                    Err(e) => {
                        // A write that fails is a real fault, not a lost
                        // source: report it and let the next pass retry.
                        tracing::warn!(
                            conversation_id = candidate.id,
                            error = %e,
                            "token usage sync: failed to settle a lost transcript"
                        );
                        result.failed += 1;
                        keep_eligible_for_rebuild(conn, schema_changed, candidate.id).await;
                    }
                }
            }
            Ok((detail, _)) => {
                let facts = facts_from_detail(&detail, candidate.updated_at);
                match usage_service::replace_conversation_facts(
                    conn,
                    candidate.id,
                    candidate.updated_at,
                    &facts,
                )
                .await
                {
                    Ok((turns, tokens)) => {
                        result.synced += 1;
                        result.turns_written += turns as u64;
                        result.tokens_written += tokens.max(0) as u64;
                    }
                    Err(e) => {
                        tracing::warn!(
                            conversation_id = candidate.id,
                            error = %e,
                            "token usage sync: failed to write facts"
                        );
                        result.failed += 1;
                        keep_eligible_for_rebuild(conn, schema_changed, candidate.id).await;
                    }
                }
            }
            Err(e) => {
                // A transcript that moved, was deleted, or can't be parsed is
                // not a sync failure worth aborting for — the conversation
                // keeps whatever facts it already had and gets another chance
                // next pass (its stamp is deliberately not advanced).
                tracing::warn!(
                    conversation_id = candidate.id,
                    error = %e,
                    "token usage sync: failed to parse transcript"
                );
                result.failed += 1;
                keep_eligible_for_rebuild(conn, schema_changed, candidate.id).await;
            }
        }

        let now = Utc::now();
        if done == total || (now - last_emit).num_milliseconds() >= PROGRESS_INTERVAL_MS {
            last_emit = now;
            emit_event(
                emitter,
                TOKEN_USAGE_SYNC_PROGRESS_EVENT,
                TokenUsageSyncProgress {
                    done,
                    total,
                    current_title: candidate.title.clone(),
                    result: (done == total).then(|| result.clone()),
                },
            );
        }
    }

    if schema_changed {
        record_fact_schema_version(conn).await?;
    }

    Ok(result)
}

/// Keep a conversation that failed a schema rebuild eligible for the next pass.
///
/// The two real-fault arms above (a write that failed, a transcript that would
/// not parse) leave the sync stamp untouched so the conversation keeps its
/// recorded facts and gets another try. For a conversation that was already
/// stale that is enough — it stays stale and the next pass retries it. But a
/// schema rebuild also re-parses conversations that are perfectly *current*,
/// and one of those that fails would still look current afterwards: the marker
/// advances, the pass never runs again, and it keeps serving numbers from the
/// accounting this change exists to replace.
///
/// Stamping it for re-parse is what closes that door. The marker preserves both
/// the facts and `turn_count`, so the "an empty parse must not erase recorded
/// usage" guard stays armed.
///
/// A transcript that is simply *gone* does not come here — it settles instead
/// (see [`usage_service::settle_lost_source`]). Retrying only pays off for a
/// fault that might clear; re-deriving a deleted file will not start working on
/// the ninth attempt, so that case keeps the numbers it has and stops asking.
async fn keep_eligible_for_rebuild(
    conn: &sea_orm::DatabaseConnection,
    schema_changed: bool,
    conversation_id: i32,
) {
    if !schema_changed {
        return;
    }
    if let Err(e) = usage_service::mark_stale_for_reparse(conn, conversation_id).await {
        tracing::warn!(
            conversation_id,
            error = %e,
            "token usage sync: could not keep a failed conversation eligible for rebuild"
        );
    }
}

/// Mark the stored facts as produced under [`FACT_SCHEMA_VERSION`].
///
/// Written only after a pass has walked every conversation, so a sync
/// interrupted halfway leaves the marker behind and the next one resumes the
/// rebuild. Conversations that failed inside the pass are handled separately by
/// [`keep_eligible_for_rebuild`], which stamps them so the advancing marker
/// cannot strand them on the old accounting.
async fn record_fact_schema_version(
    conn: &sea_orm::DatabaseConnection,
) -> Result<(), AppCommandError> {
    app_metadata_service::upsert_value(conn, FACT_SCHEMA_VERSION_KEY, FACT_SCHEMA_VERSION)
        .await
        .map_err(AppCommandError::from)
}

// ─── Tauri commands ─────────────────────────────────────────────────────

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn token_usage_report(
    db: tauri::State<'_, crate::db::AppDatabase>,
    filter: TokenUsageFilter,
) -> Result<TokenUsageReport, AppCommandError> {
    token_usage_report_core(&db.conn, filter).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn token_usage_facets(
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<TokenUsageFacets, AppCommandError> {
    token_usage_facets_core(&db.conn).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn token_usage_status(
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<TokenUsageSyncStatus, AppCommandError> {
    token_usage_status_core(&db.conn).await
}

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn token_usage_sync(
    app: tauri::AppHandle,
    db: tauri::State<'_, crate::db::AppDatabase>,
    mode: Option<TokenUsageSyncMode>,
) -> Result<TokenUsageSyncResult, AppCommandError> {
    let emitter = EventEmitter::Tauri(app);
    token_usage_sync_core(&db.conn, &emitter, mode.unwrap_or_default()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::conversation::{DbConversationSummary, SessionStats};
    use crate::models::message::{TurnRole, TurnUsage};

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid timestamp")
            .with_timezone(&Utc)
    }

    fn row(occurred: &str, tokens: i64) -> UsageFactRow {
        UsageFactRow {
            conversation_id: 1,
            folder_id: 1,
            agent_type: "claude_code".into(),
            model: Some("claude-opus-5".into()),
            occurred_at: ts(occurred),
            input_tokens: tokens,
            output_tokens: 0,
            cache_creation_tokens: 0,
            cache_read_tokens: 0,
            total_tokens: tokens,
            duration_ms: 0,
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

    fn turn(id: &str, usage: Option<TurnUsage>, at: &str) -> MessageTurn {
        MessageTurn {
            id: id.into(),
            role: TurnRole::Assistant,
            blocks: vec![],
            timestamp: ts(at),
            usage,
            duration_ms: Some(1200),
            model: Some("  claude-opus-5  ".into()),
            completed_at: None,
        agent_message_id: None,
        }
    }

    fn usage(input: u64, output: u64, create: u64, read: u64) -> TurnUsage {
        TurnUsage {
            input_tokens: input,
            output_tokens: output,
            cache_creation_input_tokens: create,
            cache_read_input_tokens: read,
        }
    }

    fn opts<'a>(
        labels: &'a HashMap<i32, String>,
        bucket: TokenUsageBucket,
        tz: i32,
        start: Option<DateTime<Utc>>,
        end: Option<DateTime<Utc>>,
    ) -> AggregateOptions<'a> {
        AggregateOptions {
            bucket,
            tz_offset_minutes: tz,
            range_start: start,
            range_end: end,
            folder_labels: labels,
            truncated: false,
            compare_previous: false,
        }
    }

    /// Same as [`opts`] but with the comparison window switched on.
    fn opts_comparing<'a>(labels: &'a HashMap<i32, String>) -> AggregateOptions<'a> {
        AggregateOptions {
            compare_previous: true,
            ..opts(labels, TokenUsageBucket::Day, 0, None, None)
        }
    }

    #[test]
    fn facts_skip_turns_without_usage_and_zero_turns() {
        let turns = vec![
            turn("a", None, "2026-08-01T10:00:00Z"),
            turn("b", Some(usage(0, 0, 0, 0)), "2026-08-01T11:00:00Z"),
            turn("c", Some(usage(10, 5, 0, 0)), "2026-08-01T12:00:00Z"),
        ];
        let facts = facts_from_turns(&turns, None);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].turn_key, "c");
        assert_eq!(facts[0].total_tokens(), 15);
        // Model strings are trimmed so "  x  " and "x" don't split the by-model
        // breakdown into two slices.
        assert_eq!(facts[0].model.as_deref(), Some("claude-opus-5"));
        assert_eq!(facts[0].duration_ms, 1200);
    }

    #[test]
    fn facts_land_when_the_usage_was_reported_not_when_the_turn_finished() {
        // `completed_at` is the last absorbed tool result, which a single
        // long-running command can push into the next calendar day. The tokens
        // were spent when the assistant spoke, so that is where they land.
        let mut t = turn("a", Some(usage(1, 1, 0, 0)), "2026-08-01T23:50:00Z");
        t.completed_at = Some(ts("2026-08-02T09:30:00Z"));
        let facts = facts_from_turns(&[t], None);
        assert_eq!(facts[0].occurred_at, ts("2026-08-01T23:50:00Z"));
    }

    #[test]
    fn facts_fall_back_to_positional_turn_key() {
        let t = turn("", Some(usage(1, 0, 0, 0)), "2026-08-01T10:00:00Z");
        let facts = facts_from_turns(&[t], None);
        assert_eq!(facts[0].turn_key, "turn-0");
    }

    #[test]
    fn facts_backfill_the_session_model_only_where_turns_have_none() {
        // The Codex shape: usage per turn, model only at the session level.
        let mut bare = turn("a", Some(usage(10, 5, 0, 0)), "2026-08-01T10:00:00Z");
        bare.model = None;
        let facts = facts_from_turns(&[bare], Some("gpt-5.5"));
        assert_eq!(facts[0].model.as_deref(), Some("gpt-5.5"));

        // A turn that names its own model keeps it — a recorded mid-session
        // switch must not be flattened to the session default.
        let named = turn("b", Some(usage(10, 5, 0, 0)), "2026-08-01T10:00:00Z");
        let facts = facts_from_turns(&[named], Some("gpt-5.5"));
        assert_eq!(facts[0].model.as_deref(), Some("claude-opus-5"));

        // A whitespace-only session model is no model at all.
        let mut blank = turn("c", Some(usage(1, 0, 0, 0)), "2026-08-01T10:00:00Z");
        blank.model = None;
        let facts = facts_from_turns(&[blank], Some("   "));
        assert_eq!(facts[0].model, None);
    }

    // ─── Tool categorization ────────────────────────────────────────────

    #[test]
    fn categorize_tool_skips_planning_tools_that_share_tokens() {
        assert_eq!(categorize_tool("TodoWrite"), None);
        assert_eq!(categorize_tool("ToolSearch"), None);
        assert_eq!(categorize_tool("ExitPlanMode"), None);
        assert_eq!(categorize_tool("Write"), Some(ToolCategory::Edit));
    }

    #[test]
    fn categorize_tool_matches_every_documented_token() {
        assert_eq!(categorize_tool("Edit"), Some(ToolCategory::Edit));
        assert_eq!(categorize_tool("Write"), Some(ToolCategory::Edit));
        assert_eq!(categorize_tool("MultiEdit"), Some(ToolCategory::Edit));
        assert_eq!(categorize_tool("apply_patch"), Some(ToolCategory::Edit));
        assert_eq!(
            categorize_tool("str_replace_based_edit_tool"),
            Some(ToolCategory::Edit)
        );
        assert_eq!(categorize_tool("NotebookEdit"), Some(ToolCategory::Edit));
        assert_eq!(categorize_tool("patch"), Some(ToolCategory::Edit));

        assert_eq!(categorize_tool("Read"), Some(ToolCategory::Read));
        assert_eq!(categorize_tool("Grep"), Some(ToolCategory::Read));
        assert_eq!(categorize_tool("Glob"), Some(ToolCategory::Read));
        assert_eq!(categorize_tool("ls"), Some(ToolCategory::Read));
        assert_eq!(categorize_tool("list_dir"), Some(ToolCategory::Read));
        assert_eq!(categorize_tool("search_files"), Some(ToolCategory::Read));
        assert_eq!(categorize_tool("find"), Some(ToolCategory::Read));
        assert_eq!(categorize_tool("codesearch"), Some(ToolCategory::Read));

        assert_eq!(categorize_tool("Bash"), Some(ToolCategory::Shell));
        assert_eq!(categorize_tool("shell"), Some(ToolCategory::Shell));
        assert_eq!(categorize_tool("execute"), Some(ToolCategory::Shell));
        assert_eq!(categorize_tool("terminal"), Some(ToolCategory::Shell));
        assert_eq!(
            categorize_tool("run_shell_command"),
            Some(ToolCategory::Shell)
        );

        assert_eq!(categorize_tool("WebFetch"), Some(ToolCategory::Web));
        assert_eq!(categorize_tool("WebSearch"), Some(ToolCategory::Web));
        assert_eq!(categorize_tool("fetch"), Some(ToolCategory::Web));
        assert_eq!(categorize_tool("browse_page"), Some(ToolCategory::Web));
        assert_eq!(categorize_tool("web_search"), Some(ToolCategory::Web));

        assert_eq!(categorize_tool("Agent"), Some(ToolCategory::Agent));
        assert_eq!(categorize_tool("Task"), Some(ToolCategory::Agent));
        assert_eq!(categorize_tool("subagent"), Some(ToolCategory::Agent));
        assert_eq!(
            categorize_tool("delegate_to_agent"),
            Some(ToolCategory::Agent)
        );

        // Unknown tools count only toward the overall pair. ("TodoRead" is
        // NOT such an example — it contains "read" and correctly classifies
        // as Read; "ExitPlanMode" matches no category token at all.)
        assert_eq!(categorize_tool("ExitPlanMode"), None);
        assert_eq!(categorize_tool(""), None);
        assert_eq!(categorize_tool("   "), None);
    }

    #[test]
    fn categorize_tool_prefers_web_over_the_overlapping_read_token() {
        // "websearch" contains "search" (a Read token) but must classify as
        // Web — the whole point of checking Web before Read.
        assert_eq!(categorize_tool("websearch"), Some(ToolCategory::Web));
        assert_eq!(categorize_tool("WebSearchTool"), Some(ToolCategory::Web));
    }

    #[test]
    fn categorize_tool_is_case_insensitive_and_trims() {
        assert_eq!(categorize_tool("  EDIT  "), Some(ToolCategory::Edit));
        assert_eq!(categorize_tool("BASH"), Some(ToolCategory::Shell));
    }

    fn tool_use(id: &str, name: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            tool_use_id: Some(id.into()),
            tool_name: name.into(),
            input_preview: None,
            status: None,
            meta: None,
        }
    }

    fn tool_result(id: &str, is_error: bool) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: Some(id.into()),
            output_preview: None,
            is_error,
            agent_stats: None,
            images: vec![],
        }
    }

    #[test]
    fn tool_counters_count_calls_regardless_of_a_result_and_errors_only_when_matched() {
        let blocks = vec![
            tool_use("1", "Edit"),
            tool_result("1", false),
            tool_use("2", "Bash"),
            tool_result("2", true),
            tool_use("3", "UnknownTool"),
            // No matching result for call 3 — still a call, never an error.
        ];
        let c = tool_counters_from_blocks(&blocks);
        assert_eq!(c.tool_calls, 3);
        assert_eq!(c.tool_errors, 1);
        assert_eq!(c.edit_calls, 1);
        assert_eq!(c.edit_errors, 0);
        assert_eq!(c.shell_calls, 1);
        assert_eq!(c.shell_errors, 1);
        // The uncategorized call bumps only the overall pair.
        assert_eq!(c.read_calls, 0);
        assert_eq!(c.agent_calls, 0);
        assert_eq!(c.web_calls, 0);
    }

    #[test]
    fn tool_counters_ignore_a_result_with_no_matching_call() {
        let blocks = vec![tool_result("orphan", true)];
        let c = tool_counters_from_blocks(&blocks);
        assert_eq!(c.tool_calls, 0);
        assert_eq!(c.tool_errors, 0);
    }

    #[test]
    fn tool_counters_ignore_a_successful_result() {
        let blocks = vec![tool_use("1", "Read"), tool_result("1", false)];
        let c = tool_counters_from_blocks(&blocks);
        assert_eq!(c.tool_calls, 1);
        assert_eq!(c.read_calls, 1);
        assert_eq!(c.tool_errors, 0);
        assert_eq!(c.read_errors, 0);
    }

    #[test]
    fn facts_from_turns_carries_tool_counters_onto_the_fact() {
        let mut t = turn("a", Some(usage(10, 5, 0, 0)), "2026-08-01T10:00:00Z");
        t.blocks = vec![
            tool_use("1", "Write"),
            tool_result("1", true),
            tool_use("2", "Grep"),
            tool_result("2", false),
        ];
        let facts = facts_from_turns(&[t], None);
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].tool_calls, 2);
        assert_eq!(facts[0].tool_errors, 1);
        assert_eq!(facts[0].edit_calls, 1);
        assert_eq!(facts[0].edit_errors, 1);
        assert_eq!(facts[0].read_calls, 1);
        assert_eq!(facts[0].read_errors, 0);
    }

    fn detail(turns: Vec<MessageTurn>, stats: Option<SessionStats>) -> DbConversationDetail {
        DbConversationDetail {
            summary: DbConversationSummary {
                id: 1,
                folder_id: 1,
                title: None,
                title_locked: false,
                agent_type: crate::models::AgentType::Hermes,
                status: "completed".into(),
                kind: crate::db::entities::conversation::ConversationKind::Regular,
                model: Some(" hermes-model ".into()),
                git_branch: None,
                external_id: None,
                message_count: turns.len() as u32,
                child_count: 0,
                created_at: ts("2026-08-01T09:00:00Z"),
                updated_at: ts("2026-08-01T12:00:00Z"),
                pinned_at: None,
                parent_id: None,
                parent_tool_use_id: None,
                delegation_call_id: None,
                origin_cwd: None,
            },
            turns,
            session_stats: stats,
            transcript_watermark: None,
            in_flight_user_turn_id: None,
            turns_offset: None,
            turns_total: None,
            assistant_turns_before_offset: None,
            prefix_hash: None,
            uncovered_prefix_max_ts: None,
        }
    }

    fn session_stats(input: u64, output: u64) -> SessionStats {
        SessionStats {
            total_usage: Some(usage(input, output, 0, 0)),
            total_tokens: Some(input + output),
            total_duration_ms: 9_000,
            context_window_used_tokens: None,
            context_window_max_tokens: None,
            context_window_usage_percent: None,
        }
    }

    #[test]
    fn a_session_level_total_is_recorded_when_no_turn_reports_usage() {
        // Hermes' shape: every turn's usage is None, but the session row knows
        // the totals. Reporting zero would be a confident wrong number.
        let turns = vec![
            turn("a", None, "2026-08-01T10:00:00Z"),
            turn("b", None, "2026-08-01T11:00:00Z"),
        ];
        let facts = facts_from_detail(
            &detail(turns, Some(session_stats(500, 100))),
            ts("2026-08-01T12:00:00Z"),
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].turn_key, SESSION_TOTAL_TURN_KEY);
        assert_eq!(facts[0].total_tokens(), 600);
        // Attributed to the end of the conversation, not its start.
        assert_eq!(facts[0].occurred_at, ts("2026-08-01T11:00:00Z"));
        assert_eq!(facts[0].model.as_deref(), Some("hermes-model"));
        assert_eq!(facts[0].duration_ms, 9_000);
    }

    #[test]
    fn per_turn_usage_wins_over_the_session_total_so_nothing_double_counts() {
        let turns = vec![turn("a", Some(usage(10, 5, 0, 0)), "2026-08-01T10:00:00Z")];
        let facts = facts_from_detail(
            // A session total that disagrees must not be added on top.
            &detail(turns, Some(session_stats(999_999, 999_999))),
            ts("2026-08-01T12:00:00Z"),
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].turn_key, "a");
        assert_eq!(facts[0].total_tokens(), 15);
    }

    #[test]
    fn a_conversation_with_no_token_data_anywhere_contributes_nothing() {
        // Cursor's shape: no per-turn usage and no session totals either.
        let turns = vec![turn("a", None, "2026-08-01T10:00:00Z")];
        assert!(facts_from_detail(&detail(turns.clone(), None), ts("2026-08-01T12:00:00Z")).is_empty());
        // A stats block that exists but reports zeros is equally empty.
        assert!(
            facts_from_detail(&detail(turns, Some(session_stats(0, 0))), ts("2026-08-01T12:00:00Z"))
                .is_empty()
        );
    }

    fn candidate(synced_turns: Option<i32>) -> usage_service::SyncCandidate {
        usage_service::SyncCandidate {
            id: 1,
            title: None,
            updated_at: ts("2026-08-01T12:00:00Z"),
            synced_source_updated_at: Some(ts("2026-08-01T11:00:00Z")),
            synced_turn_count: synced_turns,
        }
    }

    #[test]
    fn a_full_rebuild_reparses_even_conversations_that_look_current() {
        let current = usage_service::SyncCandidate {
            synced_source_updated_at: Some(ts("2026-08-01T12:00:00Z")),
            ..candidate(Some(3))
        };
        assert!(!current.is_stale());
        assert!(!should_reparse(&current, TokenUsageSyncMode::Incremental));
        assert!(should_reparse(&current, TokenUsageSyncMode::Full));
    }

    #[test]
    fn a_full_rebuild_still_protects_an_unreadable_source() {
        // The composition that a wipe-first rebuild would have broken: `Full`
        // selects a conversation whose stamp is intact, so when its transcript
        // comes back empty the guard still has the turn count it needs and the
        // recorded facts survive.
        let current = usage_service::SyncCandidate {
            synced_source_updated_at: Some(ts("2026-08-01T12:00:00Z")),
            ..candidate(Some(5))
        };
        assert!(should_reparse(&current, TokenUsageSyncMode::Full));
        assert!(parse_lost_a_readable_source(&detail(vec![], None), &current));
    }

    #[test]
    fn a_re_import_leaves_the_guard_armed() {
        // `mark_stale_for_reparse` keeps the stamp row (and its turn count)
        // rather than deleting it, so a re-imported conversation whose
        // transcript then turns out to be unreachable keeps its facts.
        let reimported = usage_service::SyncCandidate {
            synced_source_updated_at: Some(usage_service::force_reparse_marker()),
            ..candidate(Some(5))
        };
        assert!(reimported.is_stale());
        assert!(should_reparse(
            &reimported,
            TokenUsageSyncMode::Incremental
        ));
        assert!(parse_lost_a_readable_source(
            &detail(vec![], None),
            &reimported
        ));
    }

    #[test]
    fn an_incremental_pass_reparses_only_what_moved() {
        let stale = candidate(Some(3));
        assert!(stale.is_stale());
        assert!(should_reparse(&stale, TokenUsageSyncMode::Incremental));
    }

    #[test]
    fn an_unreadable_transcript_never_erases_recorded_usage() {
        // `get_folder_conversation_core` reports a missing transcript as a
        // successful parse with zero turns; writing that through would delete
        // real history and stamp the conversation current.
        let empty = detail(vec![], None);
        assert!(parse_lost_a_readable_source(&empty, &candidate(Some(12))));
    }

    #[test]
    fn a_conversation_that_never_had_usage_is_still_stamped() {
        // Otherwise a genuinely usage-free conversation (Cursor) would be
        // re-parsed on every pass forever.
        let empty = detail(vec![], None);
        assert!(!parse_lost_a_readable_source(&empty, &candidate(Some(0))));
        assert!(!parse_lost_a_readable_source(&empty, &candidate(None)));
    }

    #[test]
    fn turns_that_parsed_fine_but_report_no_usage_are_not_treated_as_lost() {
        // Real turns came back — the source is readable, it just has no token
        // counts. That must be recorded, not retried forever.
        let parsed = detail(vec![turn("a", None, "2026-08-01T10:00:00Z")], None);
        assert!(!parse_lost_a_readable_source(&parsed, &candidate(Some(12))));

        // Session-level stats with no turns are also a real parse (Hermes).
        let hermes = detail(vec![], Some(session_stats(10, 5)));
        assert!(!parse_lost_a_readable_source(&hermes, &candidate(Some(12))));
    }

    #[test]
    fn a_session_total_with_no_turns_at_all_lands_on_the_rows_updated_at() {
        let facts = facts_from_detail(
            &detail(vec![], Some(session_stats(300, 0))),
            ts("2026-08-02T08:00:00Z"),
        );
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].occurred_at, ts("2026-08-02T08:00:00Z"));
    }

    #[test]
    fn buckets_use_the_viewers_local_day_not_utc() {
        let labels = HashMap::new();
        // 22:30 UTC on the 1st is 06:30 local on the 2nd at UTC+8.
        let rows = vec![row("2026-08-01T22:30:00Z", 100)];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Day, 480, None, None),
        );
        assert_eq!(report.series.len(), 1);
        assert_eq!(report.series[0].bucket_key, "2026-08-02");

        let utc_report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Day, 0, None, None),
        );
        assert_eq!(utc_report.series[0].bucket_key, "2026-08-01");
    }

    #[test]
    fn series_is_dense_across_the_requested_range() {
        let labels = HashMap::new();
        let rows = vec![row("2026-08-01T10:00:00Z", 100), row("2026-08-04T10:00:00Z", 50)];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(
                &labels,
                TokenUsageBucket::Day,
                0,
                Some(ts("2026-08-01T00:00:00Z")),
                Some(ts("2026-08-05T00:00:00Z")),
            ),
        );
        let keys: Vec<&str> = report.series.iter().map(|p| p.bucket_key.as_str()).collect();
        assert_eq!(keys, ["2026-08-01", "2026-08-02", "2026-08-03", "2026-08-04"]);
        assert_eq!(report.series[1].total_tokens, 0);
        assert_eq!(report.series[3].total_tokens, 50);
    }

    #[test]
    fn week_buckets_start_on_monday() {
        let labels = HashMap::new();
        // 2026-08-01 is a Saturday; its ISO week starts Monday 2026-07-27.
        let rows = vec![row("2026-08-01T10:00:00Z", 10)];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Week, 0, None, None),
        );
        assert_eq!(report.series[0].bucket_key, "2026-07-27");
    }

    #[test]
    fn month_buckets_roll_over_the_year() {
        let labels = HashMap::new();
        let rows = vec![
            row("2026-12-15T10:00:00Z", 10),
            row("2027-01-04T10:00:00Z", 20),
        ];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Month, 0, None, None),
        );
        let keys: Vec<&str> = report.series.iter().map(|p| p.bucket_key.as_str()).collect();
        assert_eq!(keys, ["2026-12", "2027-01"]);
    }

    #[test]
    fn range_end_on_a_bucket_boundary_does_not_add_an_empty_tail() {
        let labels = HashMap::new();
        let rows = vec![row("2026-08-01T10:00:00Z", 10)];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(
                &labels,
                TokenUsageBucket::Day,
                0,
                Some(ts("2026-08-01T00:00:00Z")),
                Some(ts("2026-08-02T00:00:00Z")),
            ),
        );
        assert_eq!(report.series.len(), 1);
        assert_eq!(report.series[0].bucket_key, "2026-08-01");
    }

    #[test]
    fn breakdowns_split_by_folder_agent_and_model() {
        let mut labels = HashMap::new();
        labels.insert(1, "codeg".to_string());
        labels.insert(2, "other".to_string());
        let mut rows = vec![row("2026-08-01T10:00:00Z", 100), row("2026-08-01T11:00:00Z", 300)];
        rows[1].folder_id = 2;
        rows[1].conversation_id = 2;
        rows[1].agent_type = "codex".into();
        rows[1].model = None;

        let report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Day, 0, None, None),
        );
        assert_eq!(report.totals.total_tokens, 400);
        assert_eq!(report.totals.conversation_count, 2);
        // Sorted by tokens descending.
        assert_eq!(report.by_folder[0].label, "other");
        assert_eq!(report.by_folder[0].total_tokens, 300);
        assert_eq!(report.by_agent[0].key, "codex");
        // A turn with no model still counts, under the explicit sentinel.
        assert_eq!(report.by_model[0].key, UNKNOWN_MODEL);
    }

    #[test]
    fn heatmap_uses_local_weekday_and_hour() {
        let labels = HashMap::new();
        // Sunday 2026-08-02 23:00 UTC is Monday 07:00 at UTC+8.
        let rows = vec![row("2026-08-02T23:00:00Z", 10)];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Day, 480, None, None),
        );
        assert_eq!(report.heatmap.len(), 1);
        assert_eq!(report.heatmap[0].weekday, 0);
        assert_eq!(report.heatmap[0].hour, 7);
    }

    #[test]
    fn streak_measures_consecutive_local_days() {
        let labels = HashMap::new();
        let rows = vec![
            row("2026-08-01T10:00:00Z", 1),
            row("2026-08-02T10:00:00Z", 1),
            row("2026-08-03T10:00:00Z", 1),
            // Gap on the 4th.
            row("2026-08-05T10:00:00Z", 1),
        ];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Day, 0, None, None),
        );
        assert_eq!(report.totals.active_days, 4);
        assert_eq!(report.streak.longest_days, 3);
        assert_eq!(report.streak.current_days, 1);
        assert_eq!(report.streak.current_ends_on.as_deref(), Some("2026-08-05"));
    }

    #[test]
    fn previous_window_totals_are_reported_separately() {
        let labels = HashMap::new();
        let rows = vec![row("2026-08-08T10:00:00Z", 100)];
        let previous = vec![row("2026-08-01T10:00:00Z", 40)];
        let report = aggregate_report(&rows, &previous, &opts_comparing(&labels));
        assert_eq!(report.totals.total_tokens, 100);
        assert_eq!(
            report.previous_totals.as_ref().map(|t| t.total_tokens),
            Some(40)
        );
    }

    #[test]
    fn a_requested_but_empty_comparison_window_reports_zeros_not_absence() {
        // The distinction matters downstream: zeros render as "new", while
        // `None` renders as "no comparison available".
        let labels = HashMap::new();
        let rows = vec![row("2026-08-08T10:00:00Z", 100)];
        let report = aggregate_report(&rows, &[], &opts_comparing(&labels));
        assert_eq!(
            report.previous_totals.as_ref().map(|t| t.total_tokens),
            Some(0)
        );
    }

    #[test]
    fn no_comparison_requested_means_no_previous_totals() {
        let labels = HashMap::new();
        let rows = vec![row("2026-08-08T10:00:00Z", 100)];
        let report = aggregate_report(
            &rows,
            &[row("2026-08-01T10:00:00Z", 40)],
            &opts(&labels, TokenUsageBucket::Day, 0, None, None),
        );
        assert!(report.previous_totals.is_none());
    }

    #[test]
    fn range_end_is_always_exclusive_even_when_derived_from_the_data() {
        let labels = HashMap::new();
        let rows = vec![row("2026-08-01T10:00:00Z", 10)];
        let report = aggregate_report(
            &rows,
            &[],
            &opts(&labels, TokenUsageBucket::Day, 0, None, None),
        );
        // One tick past the last recorded turn, so "last day counted" is
        // `range_end - 1ms` no matter how the bound was resolved.
        assert_eq!(report.range_end, Some(ts("2026-08-01T10:00:00.001Z")));
        assert_eq!(report.last_activity_at, Some(ts("2026-08-01T10:00:00Z")));
    }

    #[test]
    fn empty_input_yields_an_empty_report_not_a_panic() {
        let labels = HashMap::new();
        let report = aggregate_report(
            &[],
            &[],
            &opts(&labels, TokenUsageBucket::Day, 0, None, None),
        );
        assert_eq!(report.totals.total_tokens, 0);
        assert!(report.series.is_empty());
        assert!(report.previous_totals.is_none());
        assert_eq!(report.streak.longest_days, 0);
    }

    #[test]
    fn tz_offset_is_clamped_to_real_zones() {
        assert_eq!(normalize_tz_offset(999_999), 14 * 60);
        assert_eq!(normalize_tz_offset(-999_999), -12 * 60);
        assert_eq!(normalize_tz_offset(-330), -330);
    }

    /// `token_usage_sync_core` serializes on a process-global `try_lock`, so two
    /// tests calling it at once would make one fail with "already running".
    static SYNC_TESTS_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn a_changed_fact_schema_reparses_conversations_a_stamp_calls_current() {
        use crate::db::test_helpers::{fresh_in_memory_db, seed_conversation, seed_folder};

        let _serial = SYNC_TESTS_SERIAL.lock().await;
        let db = fresh_in_memory_db().await;
        let folder = seed_folder(&db, "/tmp/tu-schema").await;
        let conv = seed_conversation(&db, folder, crate::models::AgentType::ClaudeCode).await;

        // Stamp it exactly current, the state an incremental pass skips.
        let updated_at = crate::db::entities::conversation::Entity::find_by_id(conv)
            .one(&db.conn)
            .await
            .expect("query")
            .expect("row")
            .updated_at;
        usage_service::replace_conversation_facts(&db.conn, conv, updated_at, &[])
            .await
            .expect("stamp");
        assert!(!usage_service::list_sync_candidates(&db.conn).await.expect("c")[0].is_stale());

        // First pass: the recorded schema is absent, so the conversation is
        // re-parsed even though nothing about it moved. This is what carries a
        // counting fix to history the user already has — stale-tracking never
        // would, because the transcript did not change, the arithmetic did.
        let first = token_usage_sync_core(&db.conn, &EventEmitter::Noop, TokenUsageSyncMode::Incremental)
            .await
            .expect("first sync");
        assert_eq!(first.scanned, 1);
        assert_eq!(first.skipped, 0, "a schema change must not skip anything");

        assert_eq!(
            app_metadata_service::get_value(&db.conn, FACT_SCHEMA_VERSION_KEY)
                .await
                .expect("read marker")
                .as_deref(),
            Some(FACT_SCHEMA_VERSION)
        );

        // Second pass: the marker matches, so incremental is incremental again.
        let second = token_usage_sync_core(&db.conn, &EventEmitter::Noop, TokenUsageSyncMode::Incremental)
            .await
            .expect("second sync");
        assert_eq!(second.skipped, 1, "the marker must not be sticky");
    }

    #[tokio::test]
    async fn a_transcript_that_is_gone_keeps_its_facts_and_stops_being_retried() {
        use crate::db::test_helpers::{fresh_in_memory_db, seed_conversation, seed_folder};

        let _serial = SYNC_TESTS_SERIAL.lock().await;
        let db = fresh_in_memory_db().await;
        let folder = seed_folder(&db, "/tmp/tu-schema-fail").await;
        let conv = seed_conversation(&db, folder, crate::models::AgentType::ClaudeCode).await;

        // Recorded usage, stamped exactly current: the shape only a rebuild
        // would revisit, and the shape whose transcript is unreadable now.
        let updated_at = crate::db::entities::conversation::Entity::find_by_id(conv)
            .one(&db.conn)
            .await
            .expect("query")
            .expect("row")
            .updated_at;
        usage_service::replace_conversation_facts(
            &db.conn,
            conv,
            updated_at,
            &[UsageFact {
                turn_key: "t1".into(),
                occurred_at: ts("2026-08-01T10:00:00Z"),
                model: Some("claude-opus-5".into()),
                input_tokens: 100,
                output_tokens: 20,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                duration_ms: 0,
                ..Default::default()
            }],
        )
        .await
        .expect("seed facts");
        assert!(!usage_service::list_sync_candidates(&db.conn).await.expect("c")[0].is_stale());

        // The schema change selects it, the parse comes back with nothing, and
        // the lost-source guard keeps the old rows rather than erasing them.
        let first =
            token_usage_sync_core(&db.conn, &EventEmitter::Noop, TokenUsageSyncMode::Incremental)
                .await
                .expect("first sync");
        assert_eq!(first.lost, 1, "the empty parse must not be written through");
        assert_eq!(
            first.failed, 0,
            "a source that is simply gone is not a fault the user can act on"
        );
        assert_eq!(
            usage_service::fetch_facts(&db.conn, &FactQuery::default(), 100)
                .await
                .expect("facts")
                .len(),
            1,
            "its recorded usage survives"
        );

        // The regression this guards: holding the stamp open re-walked the
        // agent's whole transcript tree on every pass, failed identically, and
        // reported it — forever, because nothing here counts attempts. The
        // stamp settles instead, so the case closes after one look.
        let second =
            token_usage_sync_core(&db.conn, &EventEmitter::Noop, TokenUsageSyncMode::Incremental)
                .await
                .expect("second sync");
        assert_eq!(second.skipped, 1, "a settled conversation is not revisited");
        assert_eq!(second.lost, 0);
        assert_eq!(second.failed, 0);

        // Settled is not sealed: a re-import (or any edit that moves
        // `updated_at`) puts it back in scope for exactly one more look, which
        // settles again. That is the escape hatch — without it, a conversation
        // whose transcript really did come back could never be re-read.
        usage_service::mark_stale_for_reparse(&db.conn, conv)
            .await
            .expect("re-import");
        let third =
            token_usage_sync_core(&db.conn, &EventEmitter::Noop, TokenUsageSyncMode::Incremental)
                .await
                .expect("third sync");
        assert_eq!(third.lost, 1, "a re-import earns one more attempt");
        assert_eq!(
            usage_service::fetch_facts(&db.conn, &FactQuery::default(), 100)
                .await
                .expect("facts")
                .len(),
            1,
            "and still does not erase what it could not re-derive"
        );
    }

    #[tokio::test]
    async fn a_failed_lost_source_stamp_during_schema_rebuild_stays_retryable() {
        use crate::db::test_helpers::{fresh_in_memory_db, seed_conversation, seed_folder};
        use sea_orm::{ConnectionTrait, DbBackend, Statement};

        let _serial = SYNC_TESTS_SERIAL.lock().await;
        let db = fresh_in_memory_db().await;
        let folder = seed_folder(&db, "/tmp/tu-schema-settle-fail").await;
        let conv = seed_conversation(&db, folder, crate::models::AgentType::ClaudeCode).await;
        let updated_at = crate::db::entities::conversation::Entity::find_by_id(conv)
            .one(&db.conn)
            .await
            .expect("query")
            .expect("row")
            .updated_at;
        usage_service::replace_conversation_facts(
            &db.conn,
            conv,
            updated_at,
            &[UsageFact {
                turn_key: "t1".into(),
                occurred_at: ts("2026-08-01T10:00:00Z"),
                model: Some("claude-opus-5".into()),
                input_tokens: 100,
                output_tokens: 20,
                cache_creation_tokens: 0,
                cache_read_tokens: 0,
                duration_ms: 0,
                ..Default::default()
            }],
        )
        .await
        .expect("seed facts");

        // A schema rebuild selects this otherwise-current row. Fail only the
        // no-op current-to-current update used to settle the missing source;
        // the recovery write to the epoch marker must remain available.
        db.conn
            .execute(Statement::from_string(
                DbBackend::Sqlite,
                r#"CREATE TRIGGER fail_current_token_usage_stamp
                   BEFORE UPDATE OF source_updated_at ON token_usage_sync
                   WHEN NEW.source_updated_at = OLD.source_updated_at
                   BEGIN
                     SELECT RAISE(ABORT, 'simulated settle failure');
                   END"#
                    .to_owned(),
            ))
            .await
            .expect("install failure trigger");

        let first =
            token_usage_sync_core(&db.conn, &EventEmitter::Noop, TokenUsageSyncMode::Incremental)
                .await
                .expect("schema rebuild");
        assert_eq!(first.failed, 1);
        assert_eq!(first.lost, 0);
        let after_failure = usage_service::list_sync_candidates(&db.conn)
            .await
            .expect("candidates");
        assert!(
            after_failure[0].is_stale(),
            "a failed settle must survive the advancing schema marker"
        );

        // The trigger allows epoch-to-current, proving the next incremental
        // pass actually revisits the row and can settle it normally.
        let second =
            token_usage_sync_core(&db.conn, &EventEmitter::Noop, TokenUsageSyncMode::Incremental)
                .await
                .expect("retry");
        assert_eq!(second.failed, 0);
        assert_eq!(second.lost, 1);
        assert!(!usage_service::list_sync_candidates(&db.conn)
            .await
            .expect("candidates")[0]
            .is_stale());
        assert_eq!(
            usage_service::fetch_facts(&db.conn, &FactQuery::default(), 100)
                .await
                .expect("facts")
                .len(),
            1,
            "the failed settle and its retry must both preserve recorded usage"
        );
    }
}
