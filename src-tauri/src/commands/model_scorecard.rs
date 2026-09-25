//! Model scorecard: which model actually performs best, per category, based
//! on the user's OWN measured usage — not vendor claims.
//!
//! Two data sources are folded together:
//!   * `token_usage_turn` (via `db::service::token_usage_service::fetch_facts`)
//!     — every recorded turn, including the tool-quality counters added
//!     alongside this module (see the `m20260924_000001_token_usage_tool_counters`
//!     migration). This is the *measured* half.
//!   * the live model catalog (`acp::model_catalog`) and the bundled
//!     models.dev snapshot (`acp::opencode_catalog::bundled_catalog`) — the
//!     *declared* half: what models an agent currently offers, and their
//!     spec'd context window / pricing / capabilities.
//!
//! [`model_scorecard_core`] is a thin async shell: two reads, then a pure
//! fold ([`build_scorecard`]) over plain data — same shape as
//! `commands::token_usage::aggregate_report`, and for the same reason: a pure
//! function over a `Vec` is both exactly testable and immune to date/DB
//! landmines.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use sea_orm::DatabaseConnection;

use crate::acp::model_catalog::{ModelCatalogEntry, StoredCatalog};
use crate::acp::opencode_catalog::{self, CatalogModel, CatalogProvider};
use crate::app_error::AppCommandError;
use crate::commands::token_usage::MAX_SCANNED_FACTS;
use crate::db::service::token_usage_service::{self as usage_service, FactQuery, UsageFactRow};
use crate::models::model_scorecard::{
    BestForEntry, CategoryCounts, CategoryErrorPct, ModelScorecard, ModelScorecardEntry, ModelSpec,
    RunnerUp,
};

/// A model must have been used within this many days of "now" — OR be in the
/// live catalog as currently available — to be eligible for any `best_for`
/// ranking. Keeps a model the user tried once, eight months ago, on an agent
/// they no longer run from winning a recommendation nobody could act on.
const RECENCY_WINDOW_DAYS: i64 = 60;

/// Below this turn count a model's averages are one or two sessions, not a
/// trend — `low_sample` on the wire, and a UI caveat, not a hard cutoff.
const LOW_SAMPLE_THRESHOLD: u64 = 20;

// ─── Public entry points ────────────────────────────────────────────────

pub async fn model_scorecard_core(
    conn: &DatabaseConnection,
) -> Result<ModelScorecard, AppCommandError> {
    let rows = usage_service::fetch_facts(conn, &FactQuery::default(), MAX_SCANNED_FACTS)
        .await
        .map_err(AppCommandError::from)?;
    let catalog = crate::acp::model_catalog::load_all(conn).await;
    Ok(build_scorecard(&rows, &catalog, Utc::now()))
}

/// A compact, plain-English summary of the scorecard for an agent (or a human)
/// deciding which model to route a task to: best model per category with the
/// measured numbers and sample sizes, plus the models available per agent.
/// Capped at 1500 characters. Empty when there is no usage to summarize (a
/// fresh install, or a DB with zero recorded turns).
pub async fn routing_guide_text(conn: &DatabaseConnection) -> String {
    let card = match model_scorecard_core(conn).await {
        Ok(c) => c,
        Err(_) => return String::new(),
    };
    format_routing_guide(&card)
}

// ─── Aggregation ────────────────────────────────────────────────────────

/// Running sums for one `(agent_type, model)` group.
#[derive(Debug, Default, Clone)]
struct ModelAgg {
    conversations: HashSet<i32>,
    turns: u64,
    sum_output_all: u128,
    sum_input_all: u128,
    sum_cache_read_all: u128,
    /// Turns with `duration_ms > 0` — the denominator for `avg_turn_ms` /
    /// `output_tokens_per_s` / `p50_turn_ms`. Turns with no recorded duration
    /// would otherwise silently drag those averages toward zero.
    timed_durations: Vec<u64>,
    sum_output_timed: u128,
    tool_calls: u64,
    tool_errors: u64,
    edit_calls: u64,
    edit_errors: u64,
    read_calls: u64,
    read_errors: u64,
    shell_calls: u64,
    shell_errors: u64,
    web_calls: u64,
    web_errors: u64,
    agent_calls: u64,
    agent_errors: u64,
    last_used_at: Option<DateTime<Utc>>,
}

impl ModelAgg {
    fn add(&mut self, row: &UsageFactRow) {
        self.conversations.insert(row.conversation_id);
        self.turns += 1;
        self.sum_output_all += row.output_tokens.max(0) as u128;
        self.sum_input_all += row.input_tokens.max(0) as u128;
        self.sum_cache_read_all += row.cache_read_tokens.max(0) as u128;
        if row.duration_ms > 0 {
            self.timed_durations.push(row.duration_ms as u64);
            self.sum_output_timed += row.output_tokens.max(0) as u128;
        }
        self.tool_calls += row.tool_calls.max(0) as u64;
        self.tool_errors += row.tool_errors.max(0) as u64;
        self.edit_calls += row.edit_calls.max(0) as u64;
        self.edit_errors += row.edit_errors.max(0) as u64;
        self.read_calls += row.read_calls.max(0) as u64;
        self.read_errors += row.read_errors.max(0) as u64;
        self.shell_calls += row.shell_calls.max(0) as u64;
        self.shell_errors += row.shell_errors.max(0) as u64;
        self.web_calls += row.web_calls.max(0) as u64;
        self.web_errors += row.web_errors.max(0) as u64;
        self.agent_calls += row.agent_calls.max(0) as u64;
        self.agent_errors += row.agent_errors.max(0) as u64;
        if self.last_used_at.is_none_or(|l| row.occurred_at > l) {
            self.last_used_at = Some(row.occurred_at);
        }
    }
}

fn pct(errors: u64, calls: u64) -> Option<f64> {
    if calls == 0 {
        None
    } else {
        Some(100.0 * errors as f64 / calls as f64)
    }
}

fn median_of(mut durations: Vec<u64>) -> Option<f64> {
    if durations.is_empty() {
        return None;
    }
    durations.sort_unstable();
    let n = durations.len();
    Some(if n % 2 == 1 {
        durations[n / 2] as f64
    } else {
        (durations[n / 2 - 1] as f64 + durations[n / 2] as f64) / 2.0
    })
}

/// A scored model paired with the extra ranking inputs that don't belong on
/// the wire entry (`timed_turns`, the category error percentages needed for
/// threshold checks even when `low_sample` would otherwise hide them).
struct ScoredModel {
    entry: ModelScorecardEntry,
    timed_turns: u64,
}

/// Fold filtered fact rows plus the live catalog into the full scorecard.
/// Pure and deterministic given the same inputs (including `now`, since
/// recency eligibility depends on it) — see the module doc for why this is
/// split out of the async shell.
pub(crate) fn build_scorecard(
    rows: &[UsageFactRow],
    catalog: &HashMap<String, StoredCatalog>,
    now: DateTime<Utc>,
) -> ModelScorecard {
    let mut aggs: HashMap<(String, String), ModelAgg> = HashMap::new();
    for row in rows {
        let model_key = row.model.clone().unwrap_or_default();
        aggs.entry((row.agent_type.clone(), model_key))
            .or_default()
            .add(row);
    }

    // Every (agent, model) with recorded usage, plus every catalog-only model
    // that has never been used — the latter get an all-zero aggregate so they
    // still render as "no data yet" rather than being omitted.
    let mut keys: HashSet<(String, String)> = aggs.keys().cloned().collect();
    for (agent_wire, stored) in catalog {
        for m in &stored.models {
            // A catalog alias (`sonnet`, labelled "Sonnet 5") that resolves to
            // a model with recorded usage (`claude-sonnet-5`) is the same
            // model, not a second, empty row.
            let aliases_used = aggs
                .keys()
                .any(|(a, used)| a == agent_wire && catalog_entry_matches(used, m));
            if !aliases_used {
                keys.insert((agent_wire.clone(), m.id.clone()));
            }
        }
    }

    let mut scored: Vec<ScoredModel> = keys
        .into_iter()
        .map(|(agent_type, model)| {
            let empty = ModelAgg::default();
            let agg = aggs
                .get(&(agent_type.clone(), model.clone()))
                .unwrap_or(&empty);
            score_one(agent_type, model, agg, catalog)
        })
        .collect();

    let best_for = compute_best_for(&scored, now);

    // Stamp `strengths` back onto the winning entries.
    let mut strengths_by_key: HashMap<(String, String), Vec<String>> = HashMap::new();
    for b in &best_for {
        strengths_by_key
            .entry((b.agent_type.clone(), b.model.clone()))
            .or_default()
            .push(b.category.clone());
    }
    for s in &mut scored {
        if let Some(cats) =
            strengths_by_key.remove(&(s.entry.agent_type.clone(), s.entry.model.clone()))
        {
            s.entry.strengths = cats;
        }
    }

    // Most-used first; ties broken alphabetically so repeated calls agree.
    scored.sort_by(|a, b| {
        b.entry
            .turns
            .cmp(&a.entry.turns)
            .then_with(|| a.entry.agent_type.cmp(&b.entry.agent_type))
            .then_with(|| a.entry.model.cmp(&b.entry.model))
    });

    ModelScorecard {
        generated_at: now,
        models: scored.into_iter().map(|s| s.entry).collect(),
        best_for,
    }
}

/// Whether a live catalog entry names `model_id` (as recorded in transcripts).
/// Agents advertise either the full id, a provider-prefixed id
/// (`opencode/x`), or an alias whose label carries the version (`sonnet` /
/// "Sonnet 5" for `claude-sonnet-5`). The label slug must match a whole
/// trailing segment, so "Opus 5" never claims `claude-opus-5-5`.
fn catalog_entry_matches(model_id: &str, entry: &ModelCatalogEntry) -> bool {
    let norm = |s: &str| s.trim().to_lowercase();
    let last = |s: &str| norm(s.rsplit('/').next().unwrap_or(s));
    let id = norm(model_id);
    if id.is_empty() {
        return false;
    }
    if norm(&entry.id) == id || last(&entry.id) == last(model_id) {
        return true;
    }
    let Some(label) = entry.label.as_deref() else {
        return false;
    };
    let slug = label
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() {
        return false;
    }
    let tail = last(model_id);
    tail == slug || tail.ends_with(&format!("-{slug}"))
}

fn score_one(
    agent_type: String,
    model: String,
    agg: &ModelAgg,
    catalog: &HashMap<String, StoredCatalog>,
) -> ScoredModel {
    let catalog_entry = catalog
        .get(&agent_type)
        .and_then(|c| c.models.iter().find(|m| catalog_entry_matches(&model, m)));
    let available = catalog
        .get(&agent_type)
        .map(|c| c.models.iter().any(|m| catalog_entry_matches(&model, m)));
    let label = catalog_entry.and_then(|m| m.label.clone());

    let timed_turns = agg.timed_durations.len() as u64;
    let avg_turn_ms = if timed_turns > 0 {
        Some(agg.timed_durations.iter().sum::<u64>() as f64 / timed_turns as f64)
    } else {
        None
    };
    let p50_turn_ms = median_of(agg.timed_durations.clone());
    let sum_duration_timed: u128 = agg.timed_durations.iter().map(|&d| d as u128).sum();
    let output_tokens_per_s = if sum_duration_timed > 0 {
        Some(agg.sum_output_timed as f64 / (sum_duration_timed as f64 / 1000.0))
    } else {
        None
    };
    let output_tokens_per_turn = if agg.turns > 0 {
        Some(agg.sum_output_all as f64 / agg.turns as f64)
    } else {
        None
    };
    // Share of prompt-side tokens that were served from cache rather than
    // re-processed. `None` (not zero) when the model has no recorded
    // input/cache activity at all — a genuine "we don't know", not "0% hit".
    let cache_hit_pct = {
        let denom = agg.sum_input_all + agg.sum_cache_read_all;
        if denom > 0 {
            Some(100.0 * agg.sum_cache_read_all as f64 / denom as f64)
        } else {
            None
        }
    };

    let entry = ModelScorecardEntry {
        agent_type,
        model: model.clone(),
        label,
        available,
        conversations: agg.conversations.len() as u64,
        turns: agg.turns,
        avg_turn_ms,
        p50_turn_ms,
        output_tokens_per_s,
        output_tokens_per_turn,
        cache_hit_pct,
        tool_calls: agg.tool_calls,
        tool_error_pct: pct(agg.tool_errors, agg.tool_calls),
        category_calls: CategoryCounts {
            edit: agg.edit_calls,
            read: agg.read_calls,
            shell: agg.shell_calls,
            web: agg.web_calls,
            agent: agg.agent_calls,
        },
        category_error_pct: CategoryErrorPct {
            edit: pct(agg.edit_errors, agg.edit_calls),
            read: pct(agg.read_errors, agg.read_calls),
            shell: pct(agg.shell_errors, agg.shell_calls),
            web: pct(agg.web_errors, agg.web_calls),
            agent: pct(agg.agent_errors, agg.agent_calls),
        },
        last_used_at: agg.last_used_at,
        spec: spec_for(&model),
        strengths: Vec::new(),
        low_sample: agg.turns < LOW_SAMPLE_THRESHOLD,
    };
    ScoredModel { entry, timed_turns }
}

// ─── Spec matching (models.dev) ─────────────────────────────────────────

/// Providers preferred on an id collision across the bundled catalog, in
/// preference order. Anything else still matches — it's just outranked.
const PREFERRED_PROVIDERS: &[&str] = &["anthropic", "opencode", "openai", "openrouter"];

fn provider_rank(provider_id: &str) -> usize {
    PREFERRED_PROVIDERS
        .iter()
        .position(|p| *p == provider_id)
        .unwrap_or(PREFERRED_PROVIDERS.len())
}

static MODEL_CATALOG_SNAPSHOT: OnceLock<Vec<CatalogProvider>> = OnceLock::new();

fn model_specs() -> &'static [CatalogProvider] {
    MODEL_CATALOG_SNAPSHOT
        .get_or_init(opencode_catalog::bundled_catalog)
        .as_slice()
}

/// Find `id` across every provider, preferring [`PREFERRED_PROVIDERS`] on a
/// collision (the same model id offered by more than one provider).
fn find_by_id<'a>(providers: &'a [CatalogProvider], id: &str) -> Option<&'a CatalogModel> {
    let mut best: Option<(&CatalogModel, usize)> = None;
    for p in providers {
        for m in &p.models {
            if m.id != id {
                continue;
            }
            let rank = provider_rank(&p.id);
            if best.is_none_or(|(_, best_rank)| rank < best_rank) {
                best = Some((m, rank));
            }
        }
    }
    best.map(|(m, _)| m)
}

fn to_spec(m: &CatalogModel) -> ModelSpec {
    ModelSpec {
        context: m.context,
        reasoning: m.reasoning,
        tool_call: m.tool_call,
        cost_in: m.cost_in.unwrap_or(0.0),
        cost_out: m.cost_out.unwrap_or(0.0),
    }
}

/// Match a model id against the bundled models.dev snapshot: exact id, then
/// the id with its provider prefix stripped (`opencode/x` -> `x`), then its
/// last `/`-separated segment. Ties within one stage prefer
/// [`PREFERRED_PROVIDERS`].
pub(crate) fn spec_for(model_id: &str) -> Option<ModelSpec> {
    if model_id.is_empty() || model_id == "default" {
        return None;
    }
    let providers = model_specs();
    if let Some(m) = find_by_id(providers, model_id) {
        return Some(to_spec(m));
    }
    if let Some(idx) = model_id.find('/') {
        let stripped = &model_id[idx + 1..];
        if !stripped.is_empty() {
            if let Some(m) = find_by_id(providers, stripped) {
                return Some(to_spec(m));
            }
        }
    }
    if let Some(last) = model_id.rsplit('/').next() {
        if last != model_id && !last.is_empty() {
            if let Some(m) = find_by_id(providers, last) {
                return Some(to_spec(m));
            }
        }
    }
    None
}

// ─── best_for ranking ────────────────────────────────────────────────────

fn is_ranking_eligible(entry: &ModelScorecardEntry, now: DateTime<Utc>) -> bool {
    if entry.model.is_empty() || entry.model == "default" {
        return false;
    }
    if entry.available == Some(false) {
        return false;
    }
    let used_recently = entry
        .last_used_at
        .is_some_and(|t| (now - t).num_days() <= RECENCY_WINDOW_DAYS);
    let is_available = entry.available == Some(true);
    used_recently || is_available
}

fn is_free_model(model_id: &str, spec: Option<&ModelSpec>) -> bool {
    let id_marks_free = model_id.contains(":free") || model_id.contains("-free");
    let spec_free = spec.is_some_and(|s| s.cost_in == 0.0 && s.cost_out == 0.0);
    id_marks_free || spec_free
}

/// One candidate under consideration for a `best_for` slot.
struct Candidate {
    agent_type: String,
    model: String,
    value: f64,
    sample: u64,
    /// What the ranking actually sorts on. For error rates this is the 95%
    /// Wilson upper bound rather than the raw rate, so 0 errors in 30 calls
    /// does not outrank 2 in 600: a small sample has to earn its lead. For
    /// throughput / context it is the value itself.
    rank_key: f64,
    /// Tie-break key, compared after `value`. Direction is per-category (see
    /// [`rank_candidates`]).
    tiebreak: f64,
}

/// Sort `candidates` best-first. `maximize_value` / `maximize_tiebreak` say
/// which direction is "better" for each key; ties after both keys fall back
/// to `(agent_type, model)` so the result is fully deterministic.
fn rank_candidates(
    mut candidates: Vec<Candidate>,
    maximize_value: bool,
    maximize_tiebreak: bool,
) -> Vec<Candidate> {
    candidates.sort_by(|a, b| {
        let primary = if maximize_value {
            b.rank_key.partial_cmp(&a.rank_key)
        } else {
            a.rank_key.partial_cmp(&b.rank_key)
        }
        .unwrap_or(Ordering::Equal);
        primary
            .then_with(|| {
                if maximize_tiebreak {
                    b.tiebreak.partial_cmp(&a.tiebreak)
                } else {
                    a.tiebreak.partial_cmp(&b.tiebreak)
                }
                .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.agent_type.cmp(&b.agent_type))
            .then_with(|| a.model.cmp(&b.model))
    });
    candidates
}

/// Upper bound of the 95% Wilson score interval for an error rate given as
/// a percentage over `n` trials, returned as a percentage.
fn wilson_upper_pct(error_pct: f64, n: u64) -> f64 {
    if n == 0 {
        return 100.0;
    }
    let n = n as f64;
    let p = (error_pct / 100.0).clamp(0.0, 1.0);
    let z = 1.959_963_985_f64;
    let z2 = z * z;
    let center = p + z2 / (2.0 * n);
    let margin = z * (p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt();
    ((center + margin) / (1.0 + z2 / n) * 100.0).min(100.0)
}

fn to_best_for(category: &str, metric: &str, ranked: Vec<Candidate>) -> Option<BestForEntry> {
    let mut it = ranked.into_iter();
    let winner = it.next()?;
    let runner_up = it.next().map(|r| RunnerUp {
        agent_type: r.agent_type,
        model: r.model,
        value: r.value,
    });
    Some(BestForEntry {
        category: category.to_string(),
        agent_type: winner.agent_type,
        model: winner.model,
        metric: metric.to_string(),
        value: winner.value,
        sample: winner.sample,
        runner_up,
    })
}

fn compute_best_for(scored: &[ScoredModel], now: DateTime<Utc>) -> Vec<BestForEntry> {
    let eligible: Vec<&ScoredModel> = scored
        .iter()
        .filter(|s| is_ranking_eligible(&s.entry, now))
        .collect();

    let mut out = Vec::new();

    // edit: lowest edit error % with >= 20 edit calls; tie -> more calls.
    let cands = eligible
        .iter()
        .filter(|s| s.entry.category_calls.edit >= 20)
        .filter_map(|s| {
            s.entry.category_error_pct.edit.map(|v| Candidate {
                agent_type: s.entry.agent_type.clone(),
                model: s.entry.model.clone(),
                value: v,
                sample: s.entry.category_calls.edit,
                rank_key: wilson_upper_pct(v, s.entry.category_calls.edit),
                tiebreak: s.entry.category_calls.edit as f64,
            })
        })
        .collect();
    if let Some(b) = to_best_for("edit", "error_pct", rank_candidates(cands, false, true)) {
        out.push(b);
    }

    // explore: lowest read error % with >= 30 read calls; tie -> lower avg_turn_ms.
    let cands = eligible
        .iter()
        .filter(|s| s.entry.category_calls.read >= 30)
        .filter_map(|s| {
            s.entry.category_error_pct.read.map(|v| Candidate {
                agent_type: s.entry.agent_type.clone(),
                model: s.entry.model.clone(),
                value: v,
                sample: s.entry.category_calls.read,
                rank_key: wilson_upper_pct(v, s.entry.category_calls.read),
                tiebreak: s.entry.avg_turn_ms.unwrap_or(f64::MAX),
            })
        })
        .collect();
    if let Some(b) = to_best_for("explore", "error_pct", rank_candidates(cands, false, false)) {
        out.push(b);
    }

    // shell: lowest shell error % with >= 20 shell calls; tie -> more calls.
    let cands = eligible
        .iter()
        .filter(|s| s.entry.category_calls.shell >= 20)
        .filter_map(|s| {
            s.entry.category_error_pct.shell.map(|v| Candidate {
                agent_type: s.entry.agent_type.clone(),
                model: s.entry.model.clone(),
                value: v,
                sample: s.entry.category_calls.shell,
                rank_key: wilson_upper_pct(v, s.entry.category_calls.shell),
                tiebreak: s.entry.category_calls.shell as f64,
            })
        })
        .collect();
    if let Some(b) = to_best_for("shell", "error_pct", rank_candidates(cands, false, true)) {
        out.push(b);
    }

    // research: lowest web error % with >= 10 web calls; tie -> more calls.
    let cands = eligible
        .iter()
        .filter(|s| s.entry.category_calls.web >= 10)
        .filter_map(|s| {
            s.entry.category_error_pct.web.map(|v| Candidate {
                agent_type: s.entry.agent_type.clone(),
                model: s.entry.model.clone(),
                value: v,
                sample: s.entry.category_calls.web,
                rank_key: wilson_upper_pct(v, s.entry.category_calls.web),
                tiebreak: s.entry.category_calls.web as f64,
            })
        })
        .collect();
    if let Some(b) = to_best_for("research", "error_pct", rank_candidates(cands, false, true)) {
        out.push(b);
    }

    // fast: highest output_tokens_per_s with >= 20 timed turns; tie -> more timed turns.
    let cands = eligible
        .iter()
        .filter(|s| s.timed_turns >= 20)
        .filter_map(|s| {
            s.entry.output_tokens_per_s.map(|v| Candidate {
                agent_type: s.entry.agent_type.clone(),
                model: s.entry.model.clone(),
                value: v,
                sample: s.timed_turns,
                rank_key: v,
                tiebreak: s.timed_turns as f64,
            })
        })
        .collect();
    if let Some(b) = to_best_for(
        "fast",
        "output_tokens_per_s",
        rank_candidates(cands, true, true),
    ) {
        out.push(b);
    }

    // long_context: largest spec.context among tool-calling, eligible models.
    let cands = eligible
        .iter()
        .filter_map(|s| {
            let spec = s.entry.spec.as_ref()?;
            if !spec.tool_call {
                return None;
            }
            let context = spec.context?;
            Some(Candidate {
                agent_type: s.entry.agent_type.clone(),
                model: s.entry.model.clone(),
                value: context as f64,
                sample: s.entry.turns,
                rank_key: context as f64,
                tiebreak: s.entry.turns as f64,
            })
        })
        .collect();
    if let Some(b) = to_best_for(
        "long_context",
        "context",
        rank_candidates(cands, true, true),
    ) {
        out.push(b);
    }

    // free: lowest overall tool error % with >= 20 tool calls, among models
    // that cost nothing (spec, or a ":free"/"-free" id).
    let cands = eligible
        .iter()
        .filter(|s| s.entry.tool_calls >= 20)
        .filter(|s| is_free_model(&s.entry.model, s.entry.spec.as_ref()))
        .filter_map(|s| {
            s.entry.tool_error_pct.map(|v| Candidate {
                agent_type: s.entry.agent_type.clone(),
                model: s.entry.model.clone(),
                value: v,
                sample: s.entry.tool_calls,
                rank_key: wilson_upper_pct(v, s.entry.tool_calls),
                tiebreak: s.entry.tool_calls as f64,
            })
        })
        .collect();
    if let Some(b) = to_best_for("free", "error_pct", rank_candidates(cands, false, true)) {
        out.push(b);
    }

    out
}

// ─── routing_guide_text ──────────────────────────────────────────────────

fn format_routing_guide(card: &ModelScorecard) -> String {
    let total_turns: u64 = card.models.iter().map(|m| m.turns).sum();
    if total_turns == 0 {
        return String::new();
    }

    let mut out = String::from(
        "Model routing guide (measured from your own usage; error % = failed tool \
         calls, ranked by 95% Wilson upper bound so small samples must earn it):\n",
    );

    if card.best_for.is_empty() {
        out.push_str("No category has enough measured samples yet for a recommendation.\n");
    } else {
        for b in &card.best_for {
            let what = match b.category.as_str() {
                "edit" => "file edits",
                "explore" => "reading/searching code",
                "shell" => "running commands",
                "research" => "web fetch/search",
                "fast" => "throughput",
                "long_context" => "largest context",
                "free" => "zero-cost work",
                other => other,
            };
            let fmt_value = |v: f64| match b.metric.as_str() {
                "output_tokens_per_s" => format!("{v:.0} tok/s"),
                "context" => format!("{}k ctx", (v / 1000.0).round()),
                _ => format!("{v:.1}% errors"),
            };
            out.push_str(&format!(
                "- {} ({}): {}/{} {}, n={}",
                b.category,
                what,
                b.agent_type,
                b.model,
                fmt_value(b.value),
                b.sample
            ));
            if let Some(r) = &b.runner_up {
                out.push_str(&format!(
                    "; next {}/{} {}",
                    r.agent_type,
                    r.model,
                    fmt_value(r.value)
                ));
            }
            out.push('\n');
        }
    }

    // Models available per agent, from the live catalog side of the scorecard
    // (an entry counts as "available" only when the catalog affirmatively
    // lists it — unknown-catalog and known-absent both stay out of this line).
    let mut by_agent: HashMap<&str, Vec<&str>> = HashMap::new();
    for m in &card.models {
        if m.available == Some(true) {
            by_agent
                .entry(m.agent_type.as_str())
                .or_default()
                .push(m.model.as_str());
        }
    }
    if !by_agent.is_empty() {
        out.push_str("Available models: ");
        let mut agents: Vec<&&str> = by_agent.keys().collect();
        agents.sort();
        let parts: Vec<String> = agents
            .into_iter()
            .map(|a| {
                let mut models = by_agent[a].clone();
                models.sort_unstable();
                format!("{}: [{}]", a, models.join(", "))
            })
            .collect();
        out.push_str(&parts.join("; "));
        out.push('\n');
    }

    truncate_to_chars(&out, 1500)
}

/// Truncate `s` to at most `max_chars` characters, on a char boundary.
fn truncate_to_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

// ─── Desktop command ─────────────────────────────────────────────────────

#[cfg(feature = "tauri-runtime")]
#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn model_scorecard(
    db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<ModelScorecard, AppCommandError> {
    model_scorecard_core(&db.conn).await
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s)
            .expect("valid ts")
            .with_timezone(&Utc)
    }

    // ─── categorize_tool / spec_for ──────────────────────────────────────

    #[test]
    fn catalog_aliases_resolve_to_measured_models() {
        let entry = |id: &str, label: Option<&str>| ModelCatalogEntry {
            id: id.to_string(),
            label: label.map(str::to_string),
        };
        assert!(catalog_entry_matches(
            "claude-sonnet-5",
            &entry("sonnet", Some("Sonnet 5"))
        ));
        assert!(catalog_entry_matches(
            "claude-opus-5-5",
            &entry("opus", Some("Opus 5.5"))
        ));
        assert!(!catalog_entry_matches(
            "claude-opus-5-5",
            &entry("opus", Some("Opus 5"))
        ));
        assert!(catalog_entry_matches(
            "nemotron-3.5-lightning-free",
            &entry("opencode/nemotron-3.5-lightning-free", None)
        ));
        assert!(!catalog_entry_matches(
            "claude-haiku-4-5",
            &entry("default", None)
        ));
    }

    #[test]
    fn wilson_bound_makes_small_samples_earn_their_lead() {
        // 0/30 has a wider interval than 2/600, so the larger sample ranks first.
        assert!(wilson_upper_pct(0.0, 30) > wilson_upper_pct(2.0 / 600.0 * 100.0, 600));
        // With equal evidence, the lower raw rate still wins.
        assert!(wilson_upper_pct(1.0, 500) < wilson_upper_pct(3.0, 500));
        assert_eq!(wilson_upper_pct(5.0, 0), 100.0);
    }

    #[test]
    fn spec_for_matches_exact_id() {
        let spec = spec_for("claude-opus-5").expect("bundled snapshot has this id");
        assert!(spec.context.unwrap_or(0) > 0);
    }

    #[test]
    fn spec_for_strips_a_provider_prefix() {
        // Any id that exists bare in the snapshot should also resolve when
        // prefixed with a fake provider segment.
        let bare = "claude-opus-5";
        assert!(spec_for(bare).is_some());
        let prefixed = format!("some-provider/{bare}");
        assert_eq!(
            spec_for(&prefixed).map(|s| s.context),
            spec_for(bare).map(|s| s.context)
        );
    }

    #[test]
    fn spec_for_returns_none_for_unknown_or_empty_or_default() {
        assert!(spec_for("totally-not-a-real-model-id-xyz").is_none());
        assert!(spec_for("").is_none());
        assert!(spec_for("default").is_none());
    }

    // ─── build_scorecard: shape + basic aggregation ──────────────────────

    #[test]
    fn build_scorecard_unions_usage_and_catalog_models() {
        let rows = vec![row(
            "claude_code",
            Some("claude-sonnet-5"),
            "2026-09-01T00:00:00Z",
        )];
        let mut catalog = HashMap::new();
        catalog.insert(
            "claude_code".to_string(),
            StoredCatalog {
                models: vec![
                    crate::acp::model_catalog::ModelCatalogEntry {
                        id: "claude-sonnet-5".to_string(),
                        label: Some("Sonnet 5".to_string()),
                    },
                    crate::acp::model_catalog::ModelCatalogEntry {
                        id: "claude-opus-5".to_string(),
                        label: None,
                    },
                ],
                seen_at: ts("2026-09-20T00:00:00Z"),
            },
        );
        let card = build_scorecard(&rows, &catalog, ts("2026-09-24T00:00:00Z"));
        assert_eq!(card.models.len(), 2, "used model + catalog-only model");

        let used = card
            .models
            .iter()
            .find(|m| m.model == "claude-sonnet-5")
            .unwrap();
        assert_eq!(used.turns, 1);
        assert_eq!(used.available, Some(true));
        assert_eq!(used.label.as_deref(), Some("Sonnet 5"));

        let unused = card
            .models
            .iter()
            .find(|m| m.model == "claude-opus-5")
            .unwrap();
        assert_eq!(unused.turns, 0);
        assert!(unused.low_sample);
        assert_eq!(unused.available, Some(true));
        assert_eq!(unused.conversations, 0);
        assert!(unused.avg_turn_ms.is_none());
    }

    #[test]
    fn availability_is_none_when_the_agents_catalog_was_never_seen() {
        let rows = vec![row("hermes", Some("gpt-4o"), "2026-09-01T00:00:00Z")];
        let catalog = HashMap::new();
        let card = build_scorecard(&rows, &catalog, ts("2026-09-24T00:00:00Z"));
        assert_eq!(card.models[0].available, None);
    }

    #[test]
    fn availability_is_false_when_the_catalog_is_known_but_lacks_the_model() {
        let rows = vec![row(
            "open_code",
            Some("retired-model"),
            "2026-09-01T00:00:00Z",
        )];
        let mut catalog = HashMap::new();
        catalog.insert(
            "open_code".to_string(),
            StoredCatalog {
                models: vec![crate::acp::model_catalog::ModelCatalogEntry {
                    id: "current-model".to_string(),
                    label: None,
                }],
                seen_at: ts("2026-09-20T00:00:00Z"),
            },
        );
        let card = build_scorecard(&rows, &catalog, ts("2026-09-24T00:00:00Z"));
        assert_eq!(card.models[0].available, Some(false));
    }

    #[test]
    fn empty_model_id_is_kept_but_excluded_from_ranking() {
        // 25 turns with no model recorded at all, all "errors" via edit_calls
        // (so it WOULD win `edit` on numbers alone if it were eligible).
        let rows: Vec<UsageFactRow> = (0..25)
            .map(|i| {
                let mut r = row("claude_code", None, "2026-09-20T00:00:00Z");
                r.edit_calls = 1;
                r.conversation_id = i;
                r
            })
            .collect();
        let card = build_scorecard(&rows, &HashMap::new(), ts("2026-09-24T00:00:00Z"));
        assert_eq!(card.models.len(), 1);
        assert_eq!(card.models[0].model, "");
        assert_eq!(card.models[0].turns, 25);
        // Not ranked despite meeting the edit threshold with a perfect (0%)
        // error rate — an empty model id is excluded from ranking.
        assert!(card.best_for.iter().all(|b| b.category != "edit"));
    }

    // ─── best_for: thresholds, ties, omission, eligibility ───────────────

    fn rows_with_edit_calls(
        model: &str,
        count: u32,
        errors: u32,
        now: DateTime<Utc>,
    ) -> Vec<UsageFactRow> {
        (0..count)
            .map(|i| {
                let mut r = row("claude_code", Some(model), "2026-09-20T00:00:00Z");
                r.conversation_id = i as i32;
                r.occurred_at = now;
                r.edit_calls = 1;
                r.edit_errors = if i < errors { 1 } else { 0 };
                r
            })
            .collect()
    }

    #[test]
    fn edit_category_picks_the_lowest_error_rate_above_threshold() {
        let now = ts("2026-09-24T00:00:00Z");
        let mut rows = rows_with_edit_calls("model-a", 20, 4, now); // 20% errors
        rows.extend(rows_with_edit_calls("model-b", 20, 2, now)); // 10% errors
        let card = build_scorecard(&rows, &HashMap::new(), now);
        let edit = card
            .best_for
            .iter()
            .find(|b| b.category == "edit")
            .expect("edit entry");
        assert_eq!(edit.model, "model-b");
        assert!((edit.value - 10.0).abs() < f64::EPSILON);
        assert_eq!(edit.sample, 20);
        let runner_up = edit.runner_up.as_ref().expect("runner up");
        assert_eq!(runner_up.model, "model-a");
    }

    #[test]
    fn edit_category_omitted_below_threshold() {
        let now = ts("2026-09-24T00:00:00Z");
        let rows = rows_with_edit_calls("model-a", 19, 0, now);
        let card = build_scorecard(&rows, &HashMap::new(), now);
        assert!(card.best_for.iter().all(|b| b.category != "edit"));
    }

    #[test]
    fn edit_category_tie_breaks_on_more_calls() {
        let now = ts("2026-09-24T00:00:00Z");
        // Both at 0% errors; model-b has more calls and should win the tie.
        let mut rows = rows_with_edit_calls("model-a", 20, 0, now);
        rows.extend(rows_with_edit_calls("model-b", 30, 0, now));
        let card = build_scorecard(&rows, &HashMap::new(), now);
        let edit = card.best_for.iter().find(|b| b.category == "edit").unwrap();
        assert_eq!(edit.model, "model-b");
    }

    #[test]
    fn a_model_not_used_recently_and_not_available_is_not_ranked() {
        let old = ts("2026-01-01T00:00:00Z");
        let now = ts("2026-09-24T00:00:00Z");
        let rows = rows_with_edit_calls("stale-model", 20, 0, old);
        let card = build_scorecard(&rows, &HashMap::new(), now);
        assert!(card.best_for.iter().all(|b| b.model != "stale-model"));
    }

    #[test]
    fn availability_keeps_an_old_but_catalog_listed_model_eligible() {
        let old = ts("2026-01-01T00:00:00Z");
        let now = ts("2026-09-24T00:00:00Z");
        let rows = rows_with_edit_calls("still-offered", 20, 0, old);
        let mut catalog = HashMap::new();
        catalog.insert(
            "claude_code".to_string(),
            StoredCatalog {
                models: vec![crate::acp::model_catalog::ModelCatalogEntry {
                    id: "still-offered".to_string(),
                    label: None,
                }],
                seen_at: now,
            },
        );
        let card = build_scorecard(&rows, &catalog, now);
        let edit = card.best_for.iter().find(|b| b.category == "edit");
        assert!(
            edit.is_some(),
            "available models stay eligible regardless of recency"
        );
    }

    #[test]
    fn explore_tie_breaks_on_lower_avg_turn_ms() {
        let now = ts("2026-09-24T00:00:00Z");
        let make = |model: &str, ms: i64| -> Vec<UsageFactRow> {
            (0..30)
                .map(|i| {
                    let mut r = row("claude_code", Some(model), "2026-09-20T00:00:00Z");
                    r.conversation_id = i;
                    r.occurred_at = now;
                    r.read_calls = 1;
                    r.duration_ms = ms;
                    r
                })
                .collect()
        };
        let mut rows = make("slow-model", 5000);
        rows.extend(make("fast-model", 1000));
        let card = build_scorecard(&rows, &HashMap::new(), now);
        let explore = card
            .best_for
            .iter()
            .find(|b| b.category == "explore")
            .unwrap();
        assert_eq!(explore.model, "fast-model");
    }

    #[test]
    fn fast_category_requires_twenty_timed_turns() {
        let now = ts("2026-09-24T00:00:00Z");
        let rows: Vec<UsageFactRow> = (0..19)
            .map(|i| {
                let mut r = row("claude_code", Some("model-a"), "2026-09-20T00:00:00Z");
                r.conversation_id = i;
                r.occurred_at = now;
                r.duration_ms = 1000;
                r.output_tokens = 100;
                r
            })
            .collect();
        let card = build_scorecard(&rows, &HashMap::new(), now);
        assert!(card.best_for.iter().all(|b| b.category != "fast"));
    }

    #[test]
    fn free_category_requires_a_free_signal_and_enough_tool_calls() {
        let now = ts("2026-09-24T00:00:00Z");
        let rows: Vec<UsageFactRow> = (0..20)
            .map(|i| {
                let mut r = row(
                    "open_code",
                    Some("space-bunny-free"),
                    "2026-09-20T00:00:00Z",
                );
                r.conversation_id = i;
                r.occurred_at = now;
                r.tool_calls = 1;
                r
            })
            .collect();
        let card = build_scorecard(&rows, &HashMap::new(), now);
        let free = card.best_for.iter().find(|b| b.category == "free");
        assert!(
            free.is_some(),
            "id containing -free counts as a free signal"
        );
        assert_eq!(free.unwrap().model, "space-bunny-free");
    }

    #[test]
    fn free_category_excludes_a_paid_model_even_with_zero_errors() {
        let now = ts("2026-09-24T00:00:00Z");
        let rows: Vec<UsageFactRow> = (0..20)
            .map(|i| {
                let mut r = row("claude_code", Some("claude-opus-5"), "2026-09-20T00:00:00Z");
                r.conversation_id = i;
                r.occurred_at = now;
                r.tool_calls = 1;
                r
            })
            .collect();
        let card = build_scorecard(&rows, &HashMap::new(), now);
        assert!(card
            .best_for
            .iter()
            .all(|b| b.category != "free" || b.model != "claude-opus-5"));
    }

    #[test]
    fn routing_guide_text_is_empty_with_no_data() {
        let card = ModelScorecard {
            generated_at: ts("2026-09-24T00:00:00Z"),
            models: vec![],
            best_for: vec![],
        };
        assert_eq!(format_routing_guide(&card), "");
    }

    #[test]
    fn routing_guide_text_stays_within_the_character_cap() {
        let now = ts("2026-09-24T00:00:00Z");
        let mut rows = Vec::new();
        for n in 0..50 {
            let model = format!("model-{n}");
            for i in 0..25 {
                let mut r = row("claude_code", Some(&model), "2026-09-20T00:00:00Z");
                r.conversation_id = (n * 100 + i) as i32;
                r.occurred_at = now;
                r.edit_calls = 1;
                r.duration_ms = 500;
                rows.push(r);
            }
        }
        let card = build_scorecard(&rows, &HashMap::new(), now);
        let text = format_routing_guide(&card);
        assert!(text.chars().count() <= 1500);
    }

    // ─── JSON shape (serde) ───────────────────────────────────────────────

    #[test]
    fn scorecard_serializes_with_the_documented_field_names() {
        let rows = vec![row(
            "claude_code",
            Some("claude-sonnet-5"),
            "2026-09-01T00:00:00Z",
        )];
        let card = build_scorecard(&rows, &HashMap::new(), ts("2026-09-24T00:00:00Z"));
        let json = serde_json::to_value(&card).expect("serialize");
        assert!(json.get("generated_at").is_some());
        assert!(json.get("models").is_some());
        assert!(json.get("best_for").is_some());

        let model = &json["models"][0];
        for field in [
            "agent_type",
            "model",
            "label",
            "available",
            "conversations",
            "turns",
            "avg_turn_ms",
            "p50_turn_ms",
            "output_tokens_per_s",
            "output_tokens_per_turn",
            "cache_hit_pct",
            "tool_calls",
            "tool_error_pct",
            "category_calls",
            "category_error_pct",
            "last_used_at",
            "spec",
            "strengths",
            "low_sample",
        ] {
            assert!(model.get(field).is_some(), "missing field {field}");
        }
        for field in ["edit", "read", "shell", "web", "agent"] {
            assert!(model["category_calls"].get(field).is_some());
            assert!(model["category_error_pct"].get(field).is_some());
        }
    }
}
