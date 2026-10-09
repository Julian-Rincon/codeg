//! Glue between `delegate_to_agent` and the pure [`super::router`].
//!
//! [`plan_with`] turns the call's routing arguments (`agent_type: "auto"`,
//! `task_kind`, `difficulty`, `effort`) into a concrete agent / model / effort.
//! [`DbRouting`] feeds it with live data (scorecard: available models, quota
//! limits, measured tool errors) and persists what the router learns in
//! `app_metadata` (`phantom.router.stats`, `.profile`, `.log`).

use std::collections::{BTreeMap, HashMap};

use async_trait::async_trait;
use chrono::Utc;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::router::{
    self, CandidateModel, Difficulty, Effort, Profile, RouteInput, RouterStats, ScorecardHint,
    Signal, TaskKind,
};
use crate::db::service::app_metadata_service;
use crate::models::model_scorecard::ModelScorecardEntry;
use crate::models::AgentType;

pub const STATS_KEY: &str = "phantom.router.stats";
pub const PROFILE_KEY: &str = "phantom.router.profile";
pub const LOG_KEY: &str = "phantom.router.log";
const LOG_CAP: usize = 300;
/// A free model is only routed to once it has this much measured history:
/// the catalog lists several free models that are deprecated or broken.
const FREE_MIN_TOOL_CALLS: u64 = 20;

/// Routing-relevant arguments of one `delegate_to_agent` call.
#[derive(Debug, Clone, Default)]
pub struct RoutingArgs {
    /// `agent_type: "auto"`.
    pub auto: bool,
    pub agent_type: Option<AgentType>,
    pub model: Option<String>,
    pub task_kind: Option<TaskKind>,
    pub difficulty: Option<Difficulty>,
    pub effort: Option<Effort>,
}

/// What the router chose, as shown to the lead and stored for learning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteInfo {
    pub agent_type: String,
    pub model: String,
    pub family: String,
    pub effort: String,
    pub profile: String,
    pub task_kind: String,
    pub difficulty: String,
    pub reason: String,
    /// `true` when Phantom picked the agent (`auto`), `false` when the lead did.
    pub auto: bool,
}

#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub agent_type: AgentType,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub info: Option<RouteInfo>,
}

/// Pure routing decision over already-loaded data.
pub fn plan_with(
    args: &RoutingArgs,
    candidates: &[CandidateModel],
    stats: &RouterStats,
    hints: &[ScorecardHint],
    profile: Profile,
    rng: &mut dyn FnMut() -> f64,
) -> Result<RoutePlan, String> {
    let difficulty = args.difficulty.unwrap_or(Difficulty::Normal);
    if !args.auto {
        let agent_type = args
            .agent_type
            .ok_or_else(|| "missing agent_type".to_string())?;
        let effort = args
            .effort
            .or_else(|| args.task_kind.map(|k| router::effort_for(k, difficulty)));
        let info = args.task_kind.map(|kind| {
            let model = args.model.clone().unwrap_or_else(|| "default".to_string());
            RouteInfo {
                agent_type: agent_type.as_wire().to_string(),
                family: router::family_of(&model),
                model,
                effort: effort.map_or("default", |e| e.as_str()).to_string(),
                profile: profile.as_str().to_string(),
                task_kind: kind.as_str().to_string(),
                difficulty: difficulty.as_str().to_string(),
                reason: "agent chosen by the lead".to_string(),
                auto: false,
            }
        });
        return Ok(RoutePlan {
            agent_type,
            model: args.model.clone(),
            effort: effort.map(|e| e.as_str().to_string()),
            info,
        });
    }

    let kind = args.task_kind.unwrap_or(TaskKind::Implement);
    // An explicit model narrows `auto` to that model (on whichever agent has it).
    let narrowed: Vec<CandidateModel> = match args.model.as_deref() {
        Some(wanted) => candidates
            .iter()
            .filter(|c| {
                c.model == wanted
                    || c.model.rsplit('/').next() == Some(wanted)
                    || c.family == router::family_of(wanted)
            })
            .cloned()
            .collect(),
        None => candidates.to_vec(),
    };
    if args.model.is_some() && narrowed.is_empty() {
        return Err(format!(
            "model {:?} is not an available routing candidate; omit model or name an agent",
            args.model.as_deref().unwrap_or_default()
        ));
    }
    let pool = if args.model.is_some() { &narrowed } else { candidates };
    let input = RouteInput {
        kind,
        difficulty,
        effort: args.effort,
    };
    let decision = router::route(&input, pool, stats, hints, profile, rng)
        .ok_or_else(|| "router has no available agent/model to pick from".to_string())?;
    let info = RouteInfo {
        agent_type: decision.agent_type.as_wire().to_string(),
        model: decision.model.clone(),
        family: decision.family.clone(),
        effort: decision.effort.as_str().to_string(),
        profile: decision.profile.as_str().to_string(),
        task_kind: kind.as_str().to_string(),
        difficulty: difficulty.as_str().to_string(),
        reason: decision.reason.clone(),
        auto: true,
    };
    Ok(RoutePlan {
        agent_type: decision.agent_type,
        model: Some(decision.model),
        effort: Some(decision.effort.as_str().to_string()),
        info: Some(info),
    })
}

/// `{"difficulty", "hint"}` telling the lead how to retry a failed/bad task.
pub fn escalate_value(info: &RouteInfo) -> Value {
    let next = Difficulty::parse(&info.difficulty).and_then(router::escalate);
    match next {
        Some(d) => json!({
            "difficulty": d.as_str(),
            "hint": format!(
                "re-delegate with agent_type \"auto\", task_kind \"{}\", difficulty \"{}\" (stronger model / more effort)",
                info.task_kind,
                d.as_str()
            ),
        }),
        None => json!({
            "difficulty": info.difficulty,
            "hint": "already at the top difficulty: split the task or handle it yourself",
        }),
    }
}

/// Candidates + scorecard hints from scorecard entries. One candidate per
/// (agent, family): Claude Code aliases (`opus`, `sonnet`, …) win over pinned
/// ids, and a free model needs measured history before it is offered.
pub fn candidates_from_scorecard(
    entries: &[ModelScorecardEntry],
) -> (Vec<CandidateModel>, Vec<ScorecardHint>) {
    // Measured tool calls per (agent, family), across every id spelling.
    let mut measured: HashMap<(String, String), u64> = HashMap::new();
    // (agent, family, category) -> (calls, errors)
    let mut cats: BTreeMap<(String, String, &'static str), (u64, f64)> = BTreeMap::new();
    for e in entries {
        let family = router::family_of(&e.model);
        *measured
            .entry((e.agent_type.clone(), family.clone()))
            .or_default() += e.tool_calls;
        let pairs: [(&'static str, u64, Option<f64>); 4] = [
            ("edit", e.category_calls.edit, e.category_error_pct.edit),
            ("explore", e.category_calls.read, e.category_error_pct.read),
            ("shell", e.category_calls.shell, e.category_error_pct.shell),
            ("research", e.category_calls.web, e.category_error_pct.web),
        ];
        for (cat, calls, pct) in pairs {
            if let (true, Some(pct)) = (calls > 0, pct) {
                let slot = cats
                    .entry((e.agent_type.clone(), family.clone(), cat))
                    .or_default();
                slot.0 += calls;
                slot.1 += pct / 100.0 * calls as f64;
            }
        }
    }

    let mut chosen: BTreeMap<(String, String), &ModelScorecardEntry> = BTreeMap::new();
    for e in entries {
        if e.available != Some(true) || e.limited {
            continue;
        }
        let Some(agent) = AgentType::from_wire(&e.agent_type) else {
            continue;
        };
        // Hermes is a chat gateway, not a coding sub-agent; Grok applies its
        // model/effort through its own option ids, so `@model`/`@effort`
        // would be silently ignored and the route recorded wrong.
        if matches!(agent, AgentType::Hermes | AgentType::Grok) {
            continue;
        }
        let family = router::family_of(&e.model);
        let key = (e.agent_type.clone(), family.clone());
        if router::is_free(&e.model)
            && measured.get(&key).copied().unwrap_or(0) < FREE_MIN_TOOL_CALLS
        {
            continue;
        }
        let better = |cur: &ModelScorecardEntry| {
            // Prefer the bare family alias (`opus`), then the shorter id.
            let alias = |m: &str| m == family;
            (alias(&e.model), std::cmp::Reverse(e.model.len()))
                > (alias(&cur.model), std::cmp::Reverse(cur.model.len()))
        };
        match chosen.get(&key) {
            Some(cur) if !better(cur) => {}
            _ => {
                chosen.insert(key, e);
            }
        }
    }
    let candidates = chosen
        .into_iter()
        .filter_map(|((agent, family), e)| {
            Some(CandidateModel {
                agent_type: AgentType::from_wire(&agent)?,
                model: e.model.clone(),
                free: router::is_free(&e.model),
                family,
            })
        })
        .collect();
    let hints = cats
        .into_iter()
        .filter_map(|((agent, family, cat), (calls, errors))| {
            Some(ScorecardHint {
                agent_type: AgentType::from_wire(&agent)?,
                family,
                category: cat.to_string(),
                error_pct: if calls == 0 { 0.0 } else { errors / calls as f64 * 100.0 },
                sample: calls,
            })
        })
        .collect();
    (candidates, hints)
}

/// Error codes that come from the child model's own turn. Infrastructure
/// failures (spawn, auth, depth, canceled) say nothing about the model.
const MODEL_FAILURE_CODES: &[&str] = &[
    "subagent_error",
    "child_refusal",
    "child_max_tokens",
    "child_max_turn_requests",
    "child_empty",
    "child_unknown",
    // The agent refused the routed model (deprecated/unavailable): steer away.
    "child_rejected",
];

/// Whether a `Failed` report's code should count against the routed model.
pub fn is_model_failure(error_code: Option<&str>) -> bool {
    error_code.is_some_and(|c| MODEL_FAILURE_CODES.contains(&c))
}

/// Broker-facing routing service.
#[async_trait]
pub trait DelegationRouting: Send + Sync {
    async fn plan(&self, args: &RoutingArgs) -> Result<RoutePlan, String>;
    /// Remember which route a started task took, and which conversation
    /// delegated it (only that conversation may rate it).
    async fn remember(&self, task_id: &str, info: &RouteInfo, parent_conversation_id: i32);
    async fn route_of(&self, task_id: &str) -> Option<RouteInfo>;
    /// Record a learning signal. A task's terminal status counts once (also
    /// across restarts); it can be rated once, by its own parent conversation
    /// (`caller`; `None` for terminal statuses).
    async fn record(
        &self,
        task_id: &str,
        signal: Signal,
        caller: Option<i32>,
    ) -> Result<RouteInfo, String>;
}

/// No router (tests, or before the DB is wired): explicit agents only.
pub struct NoRouting;

#[async_trait]
impl DelegationRouting for NoRouting {
    async fn plan(&self, args: &RoutingArgs) -> Result<RoutePlan, String> {
        if args.auto {
            return Err("agent_type \"auto\" is not available here".to_string());
        }
        plan_with(args, &[], &RouterStats::default(), &[], Profile::Calidad, &mut || 1.0)
    }
    async fn remember(&self, _task_id: &str, _info: &RouteInfo, _parent: i32) {}
    async fn route_of(&self, _task_id: &str) -> Option<RouteInfo> {
        None
    }
    async fn record(&self, _: &str, _: Signal, _: Option<i32>) -> Result<RouteInfo, String> {
        Err("no router".to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LogEntry {
    at: String,
    task_id: String,
    route: RouteInfo,
    #[serde(default)]
    parent_conversation_id: Option<i32>,
    #[serde(default)]
    outcomes: Vec<String>,
}

fn signal_label(signal: Signal) -> &'static str {
    match signal {
        Signal::Completed => "completed",
        Signal::Failed => "failed",
        Signal::RatedGood => "rated_good",
        Signal::RatedBad => "rated_bad",
    }
}

/// Apply `signal` to a task's log entry; `Ok(true)` when it should also
/// update the stats, `Ok(false)` when it was already counted.
fn apply_to_entry(entry: &mut LogEntry, signal: Signal, caller: Option<i32>) -> Result<bool, String> {
    let has = |labels: &[&str]| entry.outcomes.iter().any(|o| labels.contains(&o.as_str()));
    match signal {
        Signal::Completed | Signal::Failed => {
            if has(&["completed", "failed"]) {
                return Ok(false);
            }
        }
        Signal::RatedGood | Signal::RatedBad => {
            if let (Some(caller), Some(owner)) = (caller, entry.parent_conversation_id) {
                if caller != owner {
                    return Err("this delegation belongs to another conversation".to_string());
                }
            }
            if has(&["rated_good", "rated_bad"]) {
                return Err("this delegation was already rated".to_string());
            }
        }
    }
    entry.outcomes.push(signal_label(signal).to_string());
    Ok(true)
}

/// Cap for the in-memory caches below (cleared wholesale when reached).
const CACHE_CAP: usize = 2000;
const SCORECARD_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// SQLite-backed router: live scorecard in, learned stats out.
pub struct DbRouting {
    conn: DatabaseConnection,
    /// Serializes every read-modify-write of the router's app_metadata rows.
    io: Mutex<()>,
    cache: Mutex<DbRoutingCache>,
}

#[derive(Default)]
struct DbRoutingCache {
    routes: HashMap<String, RouteInfo>,
    /// Task ids known NOT to be routed (spares a log scan on every poll).
    unrouted: std::collections::HashSet<String>,
    scorecard: Option<(std::time::Instant, Vec<ModelScorecardEntry>)>,
}

impl DbRouting {
    pub fn new(conn: DatabaseConnection) -> Self {
        Self {
            conn,
            io: Mutex::new(()),
            cache: Mutex::new(DbRoutingCache::default()),
        }
    }

    async fn load<T: for<'de> Deserialize<'de> + Default>(&self, key: &str) -> T {
        match app_metadata_service::get_value(&self.conn, key).await {
            Ok(Some(raw)) => serde_json::from_str(&raw).unwrap_or_default(),
            _ => T::default(),
        }
    }

    async fn save<T: Serialize>(&self, key: &str, value: &T) {
        if let Ok(raw) = serde_json::to_string(value) {
            if let Err(e) = app_metadata_service::upsert_value(&self.conn, key, &raw).await {
                tracing::warn!("[Router] failed to save {key}: {e}");
            }
        }
    }

    async fn profile(&self) -> Profile {
        match app_metadata_service::get_value(&self.conn, PROFILE_KEY).await {
            Ok(Some(raw)) => Profile::parse(raw.trim().trim_matches('"')).unwrap_or_default(),
            _ => Profile::default(),
        }
    }

    async fn scorecard(&self) -> Result<Vec<ModelScorecardEntry>, String> {
        if let Some((at, entries)) = &self.cache.lock().await.scorecard {
            if at.elapsed() < SCORECARD_TTL {
                return Ok(entries.clone());
            }
        }
        let card = crate::commands::model_scorecard::model_scorecard_core(&self.conn)
            .await
            .map_err(|e| format!("scorecard unavailable: {e:?}"))?;
        self.cache.lock().await.scorecard = Some((std::time::Instant::now(), card.models.clone()));
        Ok(card.models)
    }

    async fn disabled_agents(&self) -> Vec<String> {
        match crate::db::service::agent_setting_service::list(&self.conn).await {
            Ok(rows) => rows
                .into_iter()
                .filter(|row| !row.enabled)
                .filter_map(|row| serde_json::from_str::<AgentType>(&row.agent_type).ok())
                .map(|a| a.as_wire().into_owned())
                .collect(),
            Err(e) => {
                tracing::warn!("[Router] reading agent settings failed: {e}");
                Vec::new()
            }
        }
    }

    async fn cache_route(&self, task_id: &str, info: &RouteInfo) {
        let mut cache = self.cache.lock().await;
        if cache.routes.len() >= CACHE_CAP {
            cache.routes.clear();
        }
        cache.unrouted.remove(task_id);
        cache.routes.insert(task_id.to_string(), info.clone());
    }
}

#[async_trait]
impl DelegationRouting for DbRouting {
    async fn plan(&self, args: &RoutingArgs) -> Result<RoutePlan, String> {
        let profile = self.profile().await;
        if !args.auto {
            return plan_with(args, &[], &RouterStats::default(), &[], profile, &mut || 1.0);
        }
        let disabled = self.disabled_agents().await;
        let entries: Vec<ModelScorecardEntry> = self
            .scorecard()
            .await?
            .into_iter()
            .filter(|e| !disabled.contains(&e.agent_type))
            .collect();
        let (candidates, hints) = candidates_from_scorecard(&entries);
        let stats: RouterStats = self.load(STATS_KEY).await;
        let mut rng = || rand::random::<f64>();
        plan_with(args, &candidates, &stats, &hints, profile, &mut rng)
    }

    async fn remember(&self, task_id: &str, info: &RouteInfo, parent_conversation_id: i32) {
        self.cache_route(task_id, info).await;
        let _io = self.io.lock().await;
        let mut log: Vec<LogEntry> = self.load(LOG_KEY).await;
        log.push(LogEntry {
            at: Utc::now().to_rfc3339(),
            task_id: task_id.to_string(),
            route: info.clone(),
            parent_conversation_id: Some(parent_conversation_id),
            outcomes: Vec::new(),
        });
        if log.len() > LOG_CAP {
            let drop = log.len() - LOG_CAP;
            log.drain(..drop);
        }
        self.save(LOG_KEY, &log).await;
    }

    async fn route_of(&self, task_id: &str) -> Option<RouteInfo> {
        {
            let cache = self.cache.lock().await;
            if let Some(info) = cache.routes.get(task_id) {
                return Some(info.clone());
            }
            if cache.unrouted.contains(task_id) {
                return None;
            }
        }
        let log: Vec<LogEntry> = self.load(LOG_KEY).await;
        let found = log
            .into_iter()
            .rev()
            .find(|e| e.task_id == task_id)
            .map(|e| e.route);
        match &found {
            Some(info) => self.cache_route(task_id, info).await,
            None => {
                let mut cache = self.cache.lock().await;
                if cache.unrouted.len() >= CACHE_CAP {
                    cache.unrouted.clear();
                }
                cache.unrouted.insert(task_id.to_string());
            }
        }
        found
    }

    async fn record(
        &self,
        task_id: &str,
        signal: Signal,
        caller: Option<i32>,
    ) -> Result<RouteInfo, String> {
        // Cheap exit for the common case: a status poll of an unrouted task.
        if self.route_of(task_id).await.is_none() {
            return Err(
                "unknown task_id: only delegations started with task_kind or agent_type \"auto\" can be rated"
                    .to_string(),
            );
        }
        let _io = self.io.lock().await;
        let mut log: Vec<LogEntry> = self.load(LOG_KEY).await;
        let Some(entry) = log.iter_mut().rev().find(|e| e.task_id == task_id) else {
            // Evicted from the capped log: still known in memory, but its
            // outcome can no longer be tracked exactly once.
            return Err("this delegation is too old to learn from".to_string());
        };
        let count = apply_to_entry(entry, signal, caller)?;
        let info = entry.route.clone();
        self.save(LOG_KEY, &log).await;
        if !count || info.family == "default" {
            return Ok(info); // already counted, or no model to learn about
        }
        if let (Some(kind), Some(agent)) = (
            TaskKind::parse(&info.task_kind),
            AgentType::from_wire(&info.agent_type),
        ) {
            let mut stats: RouterStats = self.load(STATS_KEY).await;
            stats.record(kind, agent, &info.family, signal);
            self.save(STATS_KEY, &stats).await;
        }
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::model_scorecard::{CategoryCounts, CategoryErrorPct};

    fn entry(agent: &str, model: &str, available: bool, tool_calls: u64) -> ModelScorecardEntry {
        ModelScorecardEntry {
            agent_type: agent.to_string(),
            model: model.to_string(),
            label: None,
            available: Some(available),
            conversations: 0,
            turns: 0,
            avg_turn_ms: None,
            p50_turn_ms: None,
            output_tokens_per_s: None,
            output_tokens_per_turn: None,
            cache_hit_pct: None,
            tool_calls,
            tool_error_pct: None,
            category_calls: CategoryCounts::default(),
            category_error_pct: CategoryErrorPct::default(),
            last_used_at: None,
            spec: None,
            strengths: Vec::new(),
            low_sample: false,
            limited: false,
            limit_resets_at: None,
        }
    }

    fn args(auto: bool) -> RoutingArgs {
        RoutingArgs {
            auto,
            ..Default::default()
        }
    }

    #[test]
    fn explicit_agent_keeps_choice_and_derives_effort() {
        let mut a = args(false);
        a.agent_type = Some(AgentType::OpenCode);
        a.task_kind = Some(TaskKind::Explore);
        a.difficulty = Some(Difficulty::Trivial);
        let plan = plan_with(&a, &[], &RouterStats::default(), &[], Profile::Calidad, &mut || 1.0)
            .unwrap();
        assert_eq!(plan.agent_type, AgentType::OpenCode);
        assert_eq!(plan.model, None);
        assert_eq!(plan.effort.as_deref(), Some("low"));
        let info = plan.info.unwrap();
        assert!(!info.auto);
        assert_eq!(info.family, "default");
    }

    #[test]
    fn explicit_agent_without_hints_changes_nothing() {
        let mut a = args(false);
        a.agent_type = Some(AgentType::ClaudeCode);
        a.model = Some("opus".into());
        let plan = plan_with(&a, &[], &RouterStats::default(), &[], Profile::Calidad, &mut || 1.0)
            .unwrap();
        assert_eq!(plan.model.as_deref(), Some("opus"));
        assert_eq!(plan.effort, None);
        assert!(plan.info.is_none());
    }

    #[test]
    fn auto_routes_plan_hard_to_opus_with_xhigh() {
        let entries = vec![
            entry("claude_code", "opus", true, 0),
            entry("claude_code", "sonnet", true, 0),
            entry("claude_code", "claude-sonnet-5", true, 3000),
            entry("open_code", "opencode/space-bunny-free", true, 0),
            entry("open_code", "space-bunny-free", false, 619),
            entry("open_code", "opencode/exo-free", true, 0),
        ];
        let (candidates, _) = candidates_from_scorecard(&entries);
        let ids: Vec<String> = candidates
            .iter()
            .map(|c| format!("{}/{}", c.agent_type.as_wire(), c.model))
            .collect();
        // Alias wins for sonnet; unmeasured exo-free is dropped; measured free kept.
        assert!(ids.contains(&"claude_code/sonnet".to_string()), "{ids:?}");
        assert!(!ids.iter().any(|i| i.contains("claude-sonnet-5")), "{ids:?}");
        assert!(!ids.iter().any(|i| i.contains("exo-free")), "{ids:?}");
        assert!(ids.contains(&"open_code/opencode/space-bunny-free".to_string()), "{ids:?}");

        let mut a = args(true);
        a.task_kind = Some(TaskKind::Plan);
        a.difficulty = Some(Difficulty::Hard);
        let plan = plan_with(&a, &candidates, &RouterStats::default(), &[], Profile::Calidad, &mut || 1.0)
            .unwrap();
        assert_eq!(plan.agent_type, AgentType::ClaudeCode);
        assert_eq!(plan.model.as_deref(), Some("opus"));
        assert_eq!(plan.effort.as_deref(), Some("xhigh"));
        assert!(plan.info.unwrap().auto);
    }

    #[test]
    fn limited_and_hermes_models_are_not_candidates() {
        let mut limited = entry("claude_code", "opus", true, 0);
        limited.limited = true;
        let entries = vec![limited, entry("hermes", "gpt-4o", true, 500)];
        let (candidates, _) = candidates_from_scorecard(&entries);
        assert!(candidates.is_empty());
    }

    #[test]
    fn auto_with_no_candidates_is_an_error() {
        let mut a = args(true);
        a.task_kind = Some(TaskKind::Shell);
        assert!(plan_with(&a, &[], &RouterStats::default(), &[], Profile::Calidad, &mut || 1.0).is_err());
    }

    #[test]
    fn escalate_value_points_one_level_up() {
        let info = RouteInfo {
            agent_type: "claude_code".into(),
            model: "sonnet".into(),
            family: "sonnet".into(),
            effort: "medium".into(),
            profile: "calidad".into(),
            task_kind: "implement".into(),
            difficulty: "normal".into(),
            reason: String::new(),
            auto: true,
        };
        assert_eq!(escalate_value(&info)["difficulty"], "hard");
        let top = RouteInfo {
            difficulty: "long".into(),
            ..info
        };
        assert_eq!(escalate_value(&top)["difficulty"], "long");
    }

    fn log_entry(parent: Option<i32>) -> LogEntry {
        LogEntry {
            at: String::new(),
            task_id: "t".into(),
            route: RouteInfo {
                agent_type: "claude_code".into(),
                model: "sonnet".into(),
                family: "sonnet".into(),
                effort: "medium".into(),
                profile: "calidad".into(),
                task_kind: "implement".into(),
                difficulty: "normal".into(),
                reason: String::new(),
                auto: true,
            },
            parent_conversation_id: parent,
            outcomes: Vec::new(),
        }
    }

    #[test]
    fn terminal_counts_once_even_after_restart() {
        let mut e = log_entry(Some(7));
        assert_eq!(apply_to_entry(&mut e, Signal::Completed, None), Ok(true));
        // A second poll (or the same poll after a restart, from the persisted log).
        assert_eq!(apply_to_entry(&mut e, Signal::Completed, None), Ok(false));
        assert_eq!(apply_to_entry(&mut e, Signal::Failed, None), Ok(false));
        assert_eq!(e.outcomes, vec!["completed"]);
    }

    #[test]
    fn rating_once_and_only_by_the_owner() {
        let mut e = log_entry(Some(7));
        assert!(apply_to_entry(&mut e, Signal::RatedBad, Some(8)).is_err());
        assert_eq!(apply_to_entry(&mut e, Signal::RatedGood, Some(7)), Ok(true));
        assert!(apply_to_entry(&mut e, Signal::RatedBad, Some(7)).is_err());
        assert_eq!(e.outcomes, vec!["rated_good"]);
    }

    #[test]
    fn only_model_turn_failures_count() {
        assert!(is_model_failure(Some("child_empty")));
        assert!(is_model_failure(Some("child_rejected")));
        assert!(!is_model_failure(Some("spawn_failed")));
        assert!(!is_model_failure(Some("child_auth_required")));
        assert!(!is_model_failure(None));
    }

    #[test]
    fn auto_with_unknown_model_is_an_error_not_a_silent_fallback() {
        let candidates = vec![CandidateModel {
            agent_type: AgentType::ClaudeCode,
            model: "sonnet".into(),
            family: "sonnet".into(),
            free: false,
        }];
        let mut a = args(true);
        a.model = Some("no-such-model".into());
        assert!(plan_with(&a, &candidates, &RouterStats::default(), &[], Profile::Calidad, &mut || 1.0).is_err());
        a.model = Some("claude-sonnet-5".into()); // same family
        assert!(plan_with(&a, &candidates, &RouterStats::default(), &[], Profile::Calidad, &mut || 1.0).is_ok());
    }

    #[test]
    fn grok_is_not_a_candidate() {
        let entries = vec![entry("grok", "grok-4", true, 0)];
        assert!(candidates_from_scorecard(&entries).0.is_empty());
    }
}

