//! Delegation router: given what kind of task a leader agent wants to hand
//! off and how hard it is, pick the sub-agent, model and reasoning effort.
//!
//! Pure decision logic — no I/O, no DB, no async. Callers feed it the live
//! candidate catalog, the learned [`RouterStats`], scorecard hints and the
//! active [`Profile`]; persistence and wiring live in the listener.
//!
//! Each candidate `(agent, model)` is scored as `wq·Q − wc·C − wl·L`:
//! - `Q` is the posterior mean of a Beta-like estimate: a per-`(task_kind,
//!   family)` prior worth [`PRIOR_WEIGHT`] observations plus measured results,
//!   nudged by the measured tool-error rate from the scorecard;
//! - `C` / `L` are static per-family cost / latency in `0..=1`, with cost
//!   scaled by the reasoning effort.
//!
//! Hard/long tasks are gated to strong families, and a small exploration
//! step (injectable RNG) keeps the stats from freezing on one choice.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::models::AgentType;

/// Prior strength, in pseudo-observations.
const PRIOR_WEIGHT: f64 = 8.0;
/// Prior quality for a family that is not in the priors table.
const UNKNOWN_PRIOR: f64 = 0.6;
/// Probability of picking among near-best candidates instead of the best.
const EXPLORE_PROB: f64 = 0.1;
/// "Near-best" window for exploration, in score units.
const EXPLORE_MARGIN: f64 = 0.05;
/// Minimum scorecard sample before its error rate adjusts quality.
const SCORECARD_MIN_SAMPLE: u64 = 20;
/// Quality lost at a 100% measured tool-error rate.
const SCORECARD_WEIGHT: f64 = 0.5;
/// A free model may take `hard` tasks once it has this many measured results…
const FREE_PROVEN_MIN_N: u32 = 15;
/// …and at least this quality.
const FREE_PROVEN_MIN_Q: f64 = 0.85;

/// Declares a unit enum with snake_case serde, a trimmed case-insensitive
/// `parse`, `as_str` and an `ALL` list. Variant attributes (e.g. `#[default]`)
/// pass through.
macro_rules! str_enum {
    (
        $(#[$meta:meta])*
        $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident => $s:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $( $(#[$vmeta])* $variant ),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn parse(s: &str) -> Option<Self> {
                match s.trim().to_ascii_lowercase().as_str() {
                    $( $s => Some($name::$variant), )+
                    _ => None,
                }
            }

            pub fn as_str(&self) -> &'static str {
                match self {
                    $( $name::$variant => $s ),+
                }
            }
        }
    };
}

str_enum!(
    /// What the delegated work is.
    TaskKind {
        Plan => "plan",
        Implement => "implement",
        Refactor => "refactor",
        Debug => "debug",
        Review => "review",
        Test => "test",
        Explore => "explore",
        Research => "research",
        Shell => "shell",
        Docs => "docs",
        Quick => "quick",
    }
);

str_enum!(
    /// How demanding the delegated work is.
    Difficulty {
        Trivial => "trivial",
        Normal => "normal",
        Hard => "hard",
        Long => "long",
    }
);

str_enum!(
    /// Reasoning effort, ordered low → max.
    #[derive(PartialOrd, Ord)]
    Effort {
        Low => "low",
        Medium => "medium",
        High => "high",
        Xhigh => "xhigh",
        Max => "max",
    }
);

str_enum!(
    /// Weighting preset for quality vs cost vs latency.
    #[derive(Default)]
    Profile {
        #[default]
        Calidad => "calidad",
        Equilibrado => "equilibrado",
    }
);

str_enum!(
    /// Outcome of a routed delegation, fed back into [`RouterStats`].
    Signal {
        Completed => "completed",
        Failed => "failed",
        RatedGood => "rated_good",
        RatedBad => "rated_bad",
    }
);

impl Profile {
    /// `(wq, wc, wl)`: weights of quality, cost and latency in the score.
    pub fn weights(&self) -> (f64, f64, f64) {
        match self {
            Profile::Calidad => (0.8, 0.12, 0.08),
            Profile::Equilibrado => (0.5, 0.35, 0.15),
        }
    }
}

impl Effort {
    /// One level up, capped at `Xhigh` (`Max` is never reached automatically).
    fn step_up(self) -> Self {
        match self {
            Effort::Low => Effort::Medium,
            Effort::Medium => Effort::High,
            Effort::High | Effort::Xhigh => Effort::Xhigh,
            Effort::Max => Effort::Max,
        }
    }

    /// One level down, floored at `Low`.
    fn step_down(self) -> Self {
        match self {
            Effort::Max => Effort::Xhigh,
            Effort::Xhigh => Effort::High,
            Effort::High => Effort::Medium,
            Effort::Medium | Effort::Low => Effort::Low,
        }
    }

    fn cost_multiplier(self) -> f64 {
        match self {
            Effort::Low => 0.6,
            Effort::Medium => 0.8,
            Effort::High => 1.0,
            Effort::Xhigh => 1.3,
            Effort::Max => 1.7,
        }
    }
}

/// Free-tier model ids carry a `-free` / `:free` marker.
pub fn is_free(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("-free") || m.contains(":free")
}

/// Group a concrete model id into a stats family, so measurements of
/// `claude-sonnet-5` and `sonnet` count together.
pub fn family_of(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    if is_free(&m) {
        let tail = m.rsplit('/').next().unwrap_or(&m);
        return format!("free:{tail}");
    }
    for family in ["fable", "opus", "sonnet", "haiku"] {
        if m.contains(family) {
            return family.to_string();
        }
    }
    m
}

/// Resolved view of a family string used for priors, cost, latency and gates.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FamilyClass {
    Free,
    Haiku,
    Sonnet,
    Opus,
    Fable,
    Other,
}

impl FamilyClass {
    fn of(family: &str, free: bool) -> Self {
        if free || family.starts_with("free:") {
            return FamilyClass::Free;
        }
        match family {
            "fable" => FamilyClass::Fable,
            "opus" => FamilyClass::Opus,
            "sonnet" => FamilyClass::Sonnet,
            "haiku" => FamilyClass::Haiku,
            _ => FamilyClass::Other,
        }
    }

    fn base_cost(self) -> f64 {
        match self {
            FamilyClass::Free => 0.0,
            FamilyClass::Haiku => 0.2,
            FamilyClass::Sonnet => 0.4,
            FamilyClass::Opus => 0.7,
            FamilyClass::Fable => 1.0,
            FamilyClass::Other => 0.5,
        }
    }

    fn latency(self) -> f64 {
        match self {
            FamilyClass::Free => 0.3,
            FamilyClass::Haiku => 0.2,
            FamilyClass::Sonnet => 0.4,
            FamilyClass::Opus => 0.7,
            FamilyClass::Fable => 0.8,
            FamilyClass::Other => 0.5,
        }
    }

    /// Families trusted with `hard` / `long` work.
    fn is_strong(self) -> bool {
        matches!(
            self,
            FamilyClass::Opus | FamilyClass::Fable | FamilyClass::Sonnet
        )
    }

    /// Initial quality for `kind`, before any measurement.
    fn prior(self, kind: TaskKind) -> f64 {
        // Columns: fable, opus, sonnet, haiku, free.
        let row: [f64; 5] = match kind {
            TaskKind::Plan | TaskKind::Review => [0.95, 0.95, 0.76, 0.50, 0.50],
            TaskKind::Implement | TaskKind::Refactor | TaskKind::Debug | TaskKind::Test => {
                [0.90, 0.86, 0.86, 0.60, 0.62]
            }
            TaskKind::Explore | TaskKind::Research | TaskKind::Shell => {
                [0.85, 0.82, 0.84, 0.72, 0.78]
            }
            TaskKind::Docs | TaskKind::Quick => [0.85, 0.82, 0.84, 0.75, 0.76],
        };
        match self {
            FamilyClass::Fable => row[0],
            FamilyClass::Opus => row[1],
            FamilyClass::Sonnet => row[2],
            FamilyClass::Haiku => row[3],
            FamilyClass::Free => row[4],
            FamilyClass::Other => UNKNOWN_PRIOR,
        }
    }
}

/// A routable `(agent, model)` pair from the live catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateModel {
    pub agent_type: AgentType,
    pub model: String,
    pub family: String,
    pub free: bool,
}

/// Measured tool-error rate for an `(agent, family, category)` from the
/// scorecard. `category` is one of `edit | explore | shell | research`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScorecardHint {
    pub agent_type: AgentType,
    pub family: String,
    pub category: String,
    pub error_pct: f64,
    pub sample: u64,
}

fn scorecard_category(kind: TaskKind) -> Option<&'static str> {
    match kind {
        TaskKind::Implement | TaskKind::Refactor | TaskKind::Test | TaskKind::Debug => {
            Some("edit")
        }
        TaskKind::Explore | TaskKind::Review => Some("explore"),
        TaskKind::Shell => Some("shell"),
        TaskKind::Research => Some("research"),
        TaskKind::Plan | TaskKind::Docs | TaskKind::Quick => None,
    }
}

/// Quality delta from the scorecard error rate; `0.0` when there is no
/// matching hint with enough samples.
fn scorecard_adjustment(kind: TaskKind, cand: &CandidateModel, hints: &[ScorecardHint]) -> f64 {
    let Some(category) = scorecard_category(kind) else {
        return 0.0;
    };
    hints
        .iter()
        .find(|h| {
            h.agent_type == cand.agent_type
                && h.family == cand.family
                && h.category.eq_ignore_ascii_case(category)
                && h.sample >= SCORECARD_MIN_SAMPLE
                && h.error_pct.is_finite()
        })
        .map_or(0.0, |h| {
            -SCORECARD_WEIGHT * h.error_pct.clamp(0.0, 100.0) / 100.0
        })
}

/// Measured outcomes for one `(task_kind, agent, family)`.
#[derive(Default, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Cell {
    pub good: f64,
    pub bad: f64,
    pub n: u32,
}

/// Learned outcomes keyed by `"<task_kind>|<agent wire>|<family>"`.
#[derive(Default, Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct RouterStats {
    #[serde(default)]
    pub cells: BTreeMap<String, Cell>,
}

fn cell_key(kind: TaskKind, agent: AgentType, family: &str) -> String {
    format!("{}|{}|{}", kind.as_str(), agent.as_wire(), family)
}

impl RouterStats {
    pub fn record(&mut self, kind: TaskKind, agent: AgentType, family: &str, signal: Signal) {
        let cell = self
            .cells
            .entry(cell_key(kind, agent, family))
            .or_default();
        match signal {
            Signal::Completed => cell.good += 0.5,
            Signal::Failed => cell.bad += 1.0,
            Signal::RatedGood => cell.good += 1.0,
            Signal::RatedBad => cell.bad += 1.5,
        }
        // `n` counts tasks: a rating refines a task already counted at its end.
        if matches!(signal, Signal::Completed | Signal::Failed) {
            cell.n = cell.n.saturating_add(1);
        }
    }

    pub fn cell(&self, kind: TaskKind, agent: AgentType, family: &str) -> Option<&Cell> {
        self.cells.get(&cell_key(kind, agent, family))
    }
}

/// Base reasoning effort for `kind` at `difficulty`. Never `Max`.
pub fn effort_for(kind: TaskKind, difficulty: Difficulty) -> Effort {
    let base = match difficulty {
        Difficulty::Trivial => Effort::Low,
        Difficulty::Normal => Effort::Medium,
        Difficulty::Hard => Effort::High,
        Difficulty::Long => Effort::Xhigh,
    };
    match kind {
        TaskKind::Plan | TaskKind::Review | TaskKind::Debug => base.step_up(),
        TaskKind::Quick | TaskKind::Explore => base.step_down(),
        _ => base,
    }
}

/// Next difficulty to retry at after a failure; `None` at the top.
pub fn escalate(d: Difficulty) -> Option<Difficulty> {
    match d {
        Difficulty::Trivial => Some(Difficulty::Normal),
        Difficulty::Normal => Some(Difficulty::Hard),
        Difficulty::Hard => Some(Difficulty::Long),
        Difficulty::Long => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouteInput {
    pub kind: TaskKind,
    pub difficulty: Difficulty,
    /// Explicit effort from the caller; derived from kind/difficulty if `None`.
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub agent_type: AgentType,
    pub model: String,
    pub family: String,
    pub effort: Effort,
    pub profile: Profile,
    pub quality: f64,
    pub score: f64,
    pub reason: String,
    /// Top 3 eligible candidates as `("agent/model", score)`, best first.
    pub ranking: Vec<(String, f64)>,
}

struct Scored<'a> {
    cand: &'a CandidateModel,
    class: FamilyClass,
    quality: f64,
    score: f64,
}

fn quality(
    kind: TaskKind,
    cand: &CandidateModel,
    class: FamilyClass,
    stats: &RouterStats,
    hints: &[ScorecardHint],
) -> f64 {
    let (good, bad) = stats
        .cell(kind, cand.agent_type, &cand.family)
        .map_or((0.0, 0.0), |c| (c.good, c.bad));
    let posterior = (class.prior(kind) * PRIOR_WEIGHT + good) / (PRIOR_WEIGHT + good + bad);
    (posterior + scorecard_adjustment(kind, cand, hints)).clamp(0.0, 1.0)
}

/// Whether `s` may take a task of `difficulty` (see the module gates).
fn passes_gate(
    kind: TaskKind,
    difficulty: Difficulty,
    s: &Scored<'_>,
    stats: &RouterStats,
) -> bool {
    match difficulty {
        Difficulty::Trivial | Difficulty::Normal => true,
        Difficulty::Hard | Difficulty::Long => {
            if s.class.is_strong() {
                return true;
            }
            // A free model with a strong measured record may take `hard`
            // work, but never `long`.
            difficulty == Difficulty::Hard
                && s.class == FamilyClass::Free
                && s.quality >= FREE_PROVEN_MIN_Q
                && stats
                    .cell(kind, s.cand.agent_type, &s.cand.family)
                    .is_some_and(|c| c.n >= FREE_PROVEN_MIN_N)
        }
    }
}

/// Pick the best `(agent, model)` for `input` among `candidates`.
///
/// `rng` yields uniform draws in `[0, 1)`; it is consumed only for
/// exploration on `trivial` / `normal` tasks (one draw to decide, a second to
/// choose). Returns `None` only when `candidates` is empty — if the gates
/// exclude everyone, the best ungated candidate wins.
pub fn route(
    input: &RouteInput,
    candidates: &[CandidateModel],
    stats: &RouterStats,
    hints: &[ScorecardHint],
    profile: Profile,
    rng: &mut dyn FnMut() -> f64,
) -> Option<RouteDecision> {
    if candidates.is_empty() {
        return None;
    }
    let effort = input
        .effort
        .unwrap_or_else(|| effort_for(input.kind, input.difficulty));
    let (wq, wc, wl) = profile.weights();
    let cost_multiplier = effort.cost_multiplier();

    let mut scored: Vec<Scored<'_>> = candidates
        .iter()
        .map(|cand| {
            let class = FamilyClass::of(&cand.family, cand.free);
            let quality = quality(input.kind, cand, class, stats, hints);
            let cost = (class.base_cost() * cost_multiplier).min(1.0);
            Scored {
                cand,
                class,
                quality,
                score: wq * quality - wc * cost - wl * class.latency(),
            }
        })
        .collect();
    // Best first; ties broken by (agent wire, model) so the choice is stable
    // regardless of catalog order.
    scored.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| {
                a.cand
                    .agent_type
                    .as_wire()
                    .cmp(&b.cand.agent_type.as_wire())
            })
            .then_with(|| a.cand.model.cmp(&b.cand.model))
    });

    let gated: Vec<&Scored<'_>> = scored
        .iter()
        .filter(|s| passes_gate(input.kind, input.difficulty, s, stats))
        .collect();
    let relaxed = gated.is_empty();
    let pool: Vec<&Scored<'_>> = if relaxed {
        scored.iter().collect()
    } else {
        gated
    };

    let mut chosen = 0;
    if matches!(input.difficulty, Difficulty::Trivial | Difficulty::Normal)
        && rng() < EXPLORE_PROB
    {
        let floor = pool[0].score - EXPLORE_MARGIN;
        // `pool` is sorted best-first, so the near-best window is a prefix.
        let near = pool.iter().take_while(|s| s.score >= floor).count();
        chosen = ((rng() * near as f64) as usize).min(near - 1);
    }

    let picked = pool[chosen];
    let ranking = pool
        .iter()
        .take(3)
        .map(|s| {
            (
                format!("{}/{}", s.cand.agent_type.as_wire(), s.cand.model),
                s.score,
            )
        })
        .collect();
    let mut reason = format!(
        "{}/{} → {}/{} (Q {:.2}, profile {}",
        input.kind.as_str(),
        input.difficulty.as_str(),
        picked.cand.agent_type.as_wire(),
        picked.cand.model,
        picked.quality,
        profile.as_str(),
    );
    if chosen != 0 {
        reason.push_str(", explored");
    }
    if relaxed {
        reason.push_str(", gates relaxed");
    }
    reason.push(')');

    Some(RouteDecision {
        agent_type: picked.cand.agent_type,
        model: picked.cand.model.clone(),
        family: picked.cand.family.clone(),
        effort,
        profile,
        quality: picked.quality,
        score: picked.score,
        reason,
        ranking,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    const EPS: f64 = 1e-9;

    fn approx(a: f64, b: f64) {
        assert!((a - b).abs() < EPS, "{a} != {b}");
    }

    fn cand(agent: AgentType, model: &str) -> CandidateModel {
        CandidateModel {
            agent_type: agent,
            model: model.to_string(),
            family: family_of(model),
            free: is_free(model),
        }
    }

    fn cc(model: &str) -> CandidateModel {
        cand(AgentType::ClaudeCode, model)
    }

    fn free() -> CandidateModel {
        cand(AgentType::OpenCode, "opencode/space-bunny-free")
    }

    fn input(kind: TaskKind, difficulty: Difficulty) -> RouteInput {
        RouteInput {
            kind,
            difficulty,
            effort: None,
        }
    }

    /// Route without exploration (a draw of 0.5 is never below the 0.1 gate).
    fn pick(
        kind: TaskKind,
        difficulty: Difficulty,
        candidates: &[CandidateModel],
        stats: &RouterStats,
        profile: Profile,
    ) -> RouteDecision {
        route(
            &input(kind, difficulty),
            candidates,
            stats,
            &[],
            profile,
            &mut || 0.5,
        )
        .expect("candidates are non-empty")
    }

    fn ranked(d: &RouteDecision) -> Vec<&str> {
        d.ranking.iter().map(|(name, _)| name.as_str()).collect()
    }

    #[test]
    fn enums_parse_and_as_str_roundtrip() {
        fn check<T: Copy + PartialEq + std::fmt::Debug>(
            all: &[T],
            parse: fn(&str) -> Option<T>,
            as_str: fn(&T) -> &'static str,
        ) {
            for v in all {
                let s = as_str(v);
                assert_eq!(parse(s), Some(*v), "{s}");
                assert_eq!(parse(&s.to_uppercase()), Some(*v), "{s} upper");
                assert_eq!(parse(&format!("  {s}\t")), Some(*v), "{s} padded");
            }
            assert_eq!(parse("nope"), None);
            assert_eq!(parse(""), None);
        }
        check(TaskKind::ALL, TaskKind::parse, TaskKind::as_str);
        check(Difficulty::ALL, Difficulty::parse, Difficulty::as_str);
        check(Effort::ALL, Effort::parse, Effort::as_str);
        check(Profile::ALL, Profile::parse, Profile::as_str);
        check(Signal::ALL, Signal::parse, Signal::as_str);
        assert_eq!(TaskKind::ALL.len(), 11);
        assert_eq!(Difficulty::ALL.len(), 4);
    }

    #[test]
    fn enums_serde_matches_as_str() {
        fn check<T: Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug>(
            all: &[T],
            as_str: fn(&T) -> &'static str,
        ) {
            for v in all {
                let json = serde_json::to_string(v).unwrap();
                assert_eq!(json, format!("\"{}\"", as_str(v)));
                assert_eq!(&serde_json::from_str::<T>(&json).unwrap(), v);
            }
        }
        check(TaskKind::ALL, TaskKind::as_str);
        check(Difficulty::ALL, Difficulty::as_str);
        check(Effort::ALL, Effort::as_str);
        check(Profile::ALL, Profile::as_str);
        check(Signal::ALL, Signal::as_str);
    }

    #[test]
    fn effort_is_ordered_low_to_max() {
        assert!(Effort::Low < Effort::Medium);
        assert!(Effort::Medium < Effort::High);
        assert!(Effort::High < Effort::Xhigh);
        assert!(Effort::Xhigh < Effort::Max);
    }

    #[test]
    fn profile_default_and_weights() {
        assert_eq!(Profile::default(), Profile::Calidad);
        assert_eq!(Profile::Calidad.weights(), (0.8, 0.12, 0.08));
        assert_eq!(Profile::Equilibrado.weights(), (0.5, 0.35, 0.15));
    }

    #[test]
    fn family_of_examples() {
        assert_eq!(family_of("opus"), "opus");
        assert_eq!(family_of("claude-sonnet-5"), "sonnet");
        assert_eq!(family_of("claude-opus-5-5"), "opus");
        assert_eq!(family_of("claude-haiku-4-5-20251001"), "haiku");
        assert_eq!(family_of("claude-fable-5-1"), "fable");
        assert_eq!(
            family_of("opencode/space-bunny-free"),
            "free:space-bunny-free"
        );
        assert_eq!(family_of("space-bunny-free"), "free:space-bunny-free");
        assert_eq!(family_of("vendor/model:free"), "free:model:free");
        assert_eq!(family_of("opencode/big-pickle"), "opencode/big-pickle");
        assert_eq!(family_of("Claude-Sonnet-5"), "sonnet");
        // Free wins over a family keyword in the same id.
        assert_eq!(family_of("some/opus-free"), "free:opus-free");
    }

    #[test]
    fn is_free_detects_markers() {
        assert!(is_free("opencode/space-bunny-free"));
        assert!(is_free("vendor/model:free"));
        assert!(!is_free("opencode/big-pickle"));
        assert!(!is_free("free"));
        assert!(!is_free("claude-opus-5-5"));
    }

    #[test]
    fn plan_hard_scores_follow_formula() {
        // plan/hard → effort xhigh (×1.3); calidad weights (0.7, 0.2, 0.1).
        let fable = 0.8 * 0.95 - 0.12 * 1.0f64.min(1.0 * 1.3) - 0.08 * 0.8;
        let opus = 0.8 * 0.95 - 0.12 * (0.7 * 1.3) - 0.08 * 0.7;
        let sonnet = 0.8 * 0.76 - 0.12 * (0.4 * 1.3) - 0.08 * 0.4;
        let candidates = [
            cc("fable"),
            cc("opus"),
            cc("sonnet"),
            cc("haiku"),
            free(), // gated out at hard
        ];
        let d = pick(
            TaskKind::Plan,
            Difficulty::Hard,
            &candidates,
            &RouterStats::default(),
            Profile::Calidad,
        );
        assert_eq!(d.effort, Effort::Xhigh);
        // Calibrated calidad weights: opus plans (Julian's "Opus plans" pattern).
        assert_eq!(
            ranked(&d),
            ["claude_code/opus", "claude_code/fable", "claude_code/sonnet"]
        );
        approx(d.ranking[0].1, opus);
        approx(d.ranking[1].1, fable);
        approx(d.ranking[2].1, sonnet);
        assert_eq!(d.model, "opus");
        approx(d.quality, 0.95);
        approx(d.score, opus);
    }

    #[test]
    fn plan_hard_without_sonnet_picks_opus_over_fable() {
        let fable = 0.8 * 0.95 - 0.12 * 1.0 - 0.08 * 0.8;
        let opus = 0.8 * 0.95 - 0.12 * (0.7 * 1.3) - 0.08 * 0.7;
        let candidates = [cc("fable"), cc("opus"), cc("haiku"), free()];
        let d = pick(
            TaskKind::Plan,
            Difficulty::Hard,
            &candidates,
            &RouterStats::default(),
            Profile::Calidad,
        );
        assert_eq!(d.model, "opus");
        assert_eq!(d.family, "opus");
        assert!(opus > fable);
        approx(d.score, opus);
        assert_eq!(ranked(&d), ["claude_code/opus", "claude_code/fable"]);
    }

    #[test]
    fn shell_normal_calidad_takes_sonnet_by_a_hair_equilibrado_takes_free() {
        // shell/normal → effort medium (×0.8).
        let free_score = 0.8 * 0.78 - 0.12 * 0.0 - 0.08 * 0.3;
        let sonnet = 0.8 * 0.84 - 0.12 * (0.4 * 0.8) - 0.08 * 0.4;
        let haiku = 0.8 * 0.72 - 0.12 * (0.2 * 0.8) - 0.08 * 0.2;
        let opus = 0.8 * 0.82 - 0.12 * (0.7 * 0.8) - 0.08 * 0.7;
        let fable = 0.8 * 0.85 - 0.12 * (1.0 * 0.8) - 0.08 * 0.8;
        let candidates = [cc("fable"), cc("opus"), cc("sonnet"), cc("haiku"), free()];
        let d = pick(
            TaskKind::Shell,
            Difficulty::Normal,
            &candidates,
            &RouterStats::default(),
            Profile::Calidad,
        );
        assert_eq!(d.effort, Effort::Medium);
        // Quality-first: sonnet edges the free model until the free one
        // earns measured results; the gap is inside the exploration window.
        assert_eq!(d.agent_type, AgentType::ClaudeCode);
        assert_eq!(d.model, "sonnet");
        approx(d.score, sonnet);
        assert!(sonnet > free_score && sonnet - free_score < 0.05);
        assert!(free_score > haiku && haiku > opus && opus > fable);
        assert_eq!(
            ranked(&d),
            [
                "claude_code/sonnet",
                "open_code/opencode/space-bunny-free",
                "claude_code/haiku"
            ]
        );
        let d = pick(
            TaskKind::Shell,
            Difficulty::Normal,
            &candidates,
            &RouterStats::default(),
            Profile::Equilibrado,
        );
        assert_eq!(d.family, "free:space-bunny-free");
        approx(d.quality, 0.78);
    }

    #[test]
    fn reason_has_the_documented_shape() {
        let d = pick(
            TaskKind::Implement,
            Difficulty::Hard,
            &[cc("opus")],
            &RouterStats::default(),
            Profile::Calidad,
        );
        assert_eq!(
            d.reason,
            "implement/hard → claude_code/opus (Q 0.86, profile calidad)"
        );
        assert_eq!(d.profile, Profile::Calidad);
    }

    #[test]
    fn hard_excludes_free_even_when_it_scores_higher() {
        let candidates = [free(), cc("sonnet")];
        let stats = RouterStats::default();
        // shell/hard → effort high: free is gated out whatever its score.
        let d = pick(
            TaskKind::Shell,
            Difficulty::Hard,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert_eq!(d.model, "sonnet");
        assert_eq!(ranked(&d), ["claude_code/sonnet"]);
        // The same pair at normal difficulty, cost-aware profile: free model.
        let d = pick(
            TaskKind::Shell,
            Difficulty::Normal,
            &candidates,
            &stats,
            Profile::Equilibrado,
        );
        assert_eq!(d.family, "free:space-bunny-free");
    }

    #[test]
    fn hard_and_long_exclude_haiku_and_unknown_families() {
        let mut stats = RouterStats::default();
        for _ in 0..40 {
            stats.record(TaskKind::Quick, AgentType::ClaudeCode, "haiku", Signal::RatedGood);
        }
        let candidates = [cc("haiku"), cc("sonnet"), cand(AgentType::OpenCode, "opencode/big-pickle")];
        // Trivial: the proven haiku wins (0.627 vs sonnet 0.5).
        let d = pick(
            TaskKind::Quick,
            Difficulty::Trivial,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert_eq!(d.model, "haiku");
        approx(
            d.score,
            0.8 * (0.75 * 8.0 + 40.0) / 48.0 - 0.12 * (0.2 * 0.6) - 0.08 * 0.2,
        );
        for difficulty in [Difficulty::Hard, Difficulty::Long] {
            let d = pick(TaskKind::Quick, difficulty, &candidates, &stats, Profile::Calidad);
            assert_eq!(d.model, "sonnet", "{difficulty:?}");
        }
    }

    #[test]
    fn proven_free_model_may_take_hard_but_never_long() {
        let family = "free:space-bunny-free";
        let candidates = [free(), cc("sonnet")];
        let mut stats = RouterStats::default();
        for _ in 0..30 {
            stats.record(TaskKind::Shell, AgentType::OpenCode, family, Signal::Completed);
            stats.record(TaskKind::Shell, AgentType::OpenCode, family, Signal::RatedGood);
        }
        // Q = (0.78·8 + 45) / (8 + 45) ≈ 0.967 with n = 30 tasks.
        let d = pick(
            TaskKind::Shell,
            Difficulty::Hard,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert_eq!(d.family, family);
        approx(d.quality, (0.78 * 8.0 + 45.0) / 53.0);
        // Long: great stats are not enough.
        let d = pick(
            TaskKind::Shell,
            Difficulty::Long,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert_eq!(d.model, "sonnet");
        assert_eq!(ranked(&d), ["claude_code/sonnet"]);
    }

    #[test]
    fn free_hard_exemption_needs_n_15_and_q_085() {
        let family = "free:space-bunny-free";
        let candidates = [free(), cc("sonnet")];
        let record = |good: u32, bad: u32| {
            let mut stats = RouterStats::default();
            // Each task: its terminal status plus the lead's rating.
            for _ in 0..good {
                stats.record(TaskKind::Shell, AgentType::OpenCode, family, Signal::Completed);
                stats.record(TaskKind::Shell, AgentType::OpenCode, family, Signal::RatedGood);
            }
            for _ in 0..bad {
                stats.record(TaskKind::Shell, AgentType::OpenCode, family, Signal::Failed);
                stats.record(TaskKind::Shell, AgentType::OpenCode, family, Signal::RatedBad);
            }
            stats
        };
        let model_for = |stats: &RouterStats| {
            pick(
                TaskKind::Shell,
                Difficulty::Hard,
                &candidates,
                stats,
                Profile::Calidad,
            )
            .family
        };
        // n = 14 tasks, Q ≈ 0.94: one task short.
        assert_eq!(model_for(&record(14, 0)), "sonnet");
        // n = 15 tasks, Q ≈ 0.94: allowed.
        assert_eq!(model_for(&record(15, 0)), family);
        // n = 18 tasks, Q = (6.24 + 12) / (8 + 12 + 25) ≈ 0.41: enough data, too poor.
        assert_eq!(model_for(&record(8, 10)), "sonnet");
    }

    #[test]
    fn falls_back_to_ungated_best_when_gates_leave_nobody() {
        let only_free = [
            cand(AgentType::OpenCode, "opencode/a-free"),
            cand(AgentType::OpenCode, "opencode/b-free"),
        ];
        for difficulty in [Difficulty::Hard, Difficulty::Long] {
            let d = pick(
                TaskKind::Implement,
                difficulty,
                &only_free,
                &RouterStats::default(),
                Profile::Calidad,
            );
            assert_eq!(d.model, "opencode/a-free");
            assert!(d.reason.contains("gates relaxed"), "{}", d.reason);
        }
        // Weak-only catalog (haiku) at long also still routes.
        let d = pick(
            TaskKind::Docs,
            Difficulty::Long,
            &[cc("haiku")],
            &RouterStats::default(),
            Profile::Calidad,
        );
        assert_eq!(d.model, "haiku");
    }

    #[test]
    fn no_candidates_means_no_route() {
        let d = route(
            &input(TaskKind::Plan, Difficulty::Normal),
            &[],
            &RouterStats::default(),
            &[],
            Profile::Calidad,
            &mut || panic!("rng must not be drawn"),
        );
        assert_eq!(d, None);
    }

    #[test]
    fn learns_to_switch_from_sonnet_to_opus_after_bad_ratings() {
        let candidates = [cc("claude-sonnet-5"), cc("claude-opus-5-5"), cc("claude-fable-5-1")];
        let mut stats = RouterStats::default();
        let d = pick(
            TaskKind::Implement,
            Difficulty::Hard,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert_eq!(d.model, "claude-sonnet-5");
        // Measured results for the family count for the concrete id.
        for _ in 0..10 {
            stats.record(TaskKind::Implement, AgentType::ClaudeCode, "sonnet", Signal::RatedBad);
        }
        let d = pick(
            TaskKind::Implement,
            Difficulty::Hard,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert_eq!(d.model, "claude-opus-5-5");
        assert_eq!(d.family, "opus");
        // Other task kinds are unaffected by implement results.
        let d = pick(
            TaskKind::Refactor,
            Difficulty::Hard,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert_eq!(d.model, "claude-sonnet-5");
        // Sonnet's quality after 10 bad ratings: 6.88 / (8 + 15).
        let sonnet_q = (0.86 * 8.0) / (8.0 + 15.0);
        let sonnet_score = 0.8 * sonnet_q - 0.12 * 0.4 - 0.08 * 0.4;
        let d = pick(
            TaskKind::Implement,
            Difficulty::Hard,
            &candidates,
            &stats,
            Profile::Calidad,
        );
        assert!(d.score > sonnet_score);
    }

    #[test]
    fn exploration_picks_among_near_best_with_scripted_rng() {
        // shell/normal calidad: sonnet ≈0.602, free ≈0.600, opus ≈0.538 — the
        // 0.05 window around the best holds sonnet and free only.
        let candidates = [cc("opus"), cc("sonnet"), free()];
        let stats = RouterStats::default();
        let run = |draws: &[f64]| {
            let mut queue: VecDeque<f64> = draws.iter().copied().collect();
            let mut rng = || queue.pop_front().expect("rng drawn more than scripted");
            let d = route(
                &input(TaskKind::Shell, Difficulty::Normal),
                &candidates,
                &stats,
                &[],
                Profile::Calidad,
                &mut rng,
            )
            .unwrap();
            assert!(queue.is_empty(), "unused draws: {queue:?}");
            d
        };
        // 0.5 ≥ 0.1: no exploration, a single draw, best wins.
        assert_eq!(run(&[0.5]).model, "sonnet");
        // Exactly 0.1 does not explore either.
        assert_eq!(run(&[0.1]).model, "sonnet");
        // Explore, second draw 0.99 → index 1 of [sonnet, free].
        let d = run(&[0.05, 0.99]);
        assert_eq!(d.family, "free:space-bunny-free");
        assert!(d.reason.contains("explored"), "{}", d.reason);
        approx(d.score, 0.8 * 0.78 - 0.08 * 0.3);
        // Explore, second draw 0.0 → index 0 = the best itself.
        let d = run(&[0.05, 0.0]);
        assert_eq!(d.model, "sonnet");
        assert!(!d.reason.contains("explored"), "{}", d.reason);
        // Opus is outside the window: no draw value can reach it.
        for second in [0.0, 0.3, 0.5, 0.74, 0.999_999] {
            assert_ne!(run(&[0.0, second]).model, "opus", "{second}");
        }
    }

    #[test]
    fn exploration_draws_only_on_trivial_and_normal() {
        let candidates = [cc("opus"), cc("sonnet"), free()];
        let stats = RouterStats::default();
        for difficulty in [Difficulty::Hard, Difficulty::Long] {
            // An rng that panics proves hard/long never consume a draw.
            let d = route(
                &input(TaskKind::Shell, difficulty),
                &candidates,
                &stats,
                &[],
                Profile::Calidad,
                &mut || panic!("hard/long must not explore"),
            )
            .unwrap();
            assert_eq!(d.model, "sonnet");
        }
        let mut draws = 0;
        route(
            &input(TaskKind::Shell, Difficulty::Trivial),
            &candidates,
            &stats,
            &[],
            Profile::Calidad,
            &mut || {
                draws += 1;
                0.5
            },
        )
        .unwrap();
        assert_eq!(draws, 1);
    }

    #[test]
    fn effort_for_table() {
        use Difficulty::*;
        use Effort::*;
        for &kind in TaskKind::ALL {
            let expected = match kind {
                TaskKind::Plan | TaskKind::Review | TaskKind::Debug => {
                    [Medium, High, Xhigh, Xhigh]
                }
                TaskKind::Quick | TaskKind::Explore => [Low, Low, Medium, High],
                _ => [Low, Medium, High, Xhigh],
            };
            for (d, want) in [Trivial, Normal, Hard, Long].into_iter().zip(expected) {
                assert_eq!(effort_for(kind, d), want, "{kind:?}/{d:?}");
                assert_ne!(effort_for(kind, d), Max, "{kind:?}/{d:?} must not be max");
            }
        }
    }

    #[test]
    fn explicit_effort_wins_and_drives_cost() {
        let fable = [cc("fable")];
        let stats = RouterStats::default();
        let with = |effort: Option<Effort>| {
            route(
                &RouteInput {
                    kind: TaskKind::Implement,
                    difficulty: Difficulty::Hard,
                    effort,
                },
                &fable,
                &stats,
                &[],
                Profile::Calidad,
                &mut || 0.5,
            )
            .unwrap()
        };
        // Derived: implement/hard → high.
        assert_eq!(with(None).effort, Effort::High);
        // Max is honored when asked for explicitly; fable's cost clips at 1.
        let d = with(Some(Effort::Max));
        assert_eq!(d.effort, Effort::Max);
        approx(d.score, 0.8 * 0.90 - 0.12 * 1.0 - 0.08 * 0.8);
        // Lower effort makes a non-saturated family cheaper.
        let sonnet = [cc("sonnet")];
        let score_at = |effort| {
            route(
                &RouteInput {
                    kind: TaskKind::Implement,
                    difficulty: Difficulty::Normal,
                    effort: Some(effort),
                },
                &sonnet,
                &stats,
                &[],
                Profile::Calidad,
                &mut || 0.5,
            )
            .unwrap()
            .score
        };
        approx(score_at(Effort::Low), 0.8 * 0.86 - 0.12 * (0.4 * 0.6) - 0.08 * 0.4);
        approx(score_at(Effort::Max), 0.8 * 0.86 - 0.12 * (0.4 * 1.7) - 0.08 * 0.4);
        assert!(score_at(Effort::Low) > score_at(Effort::Max));
    }

    #[test]
    fn escalate_chain() {
        assert_eq!(escalate(Difficulty::Trivial), Some(Difficulty::Normal));
        assert_eq!(escalate(Difficulty::Normal), Some(Difficulty::Hard));
        assert_eq!(escalate(Difficulty::Hard), Some(Difficulty::Long));
        assert_eq!(escalate(Difficulty::Long), None);
    }

    #[test]
    fn record_updates_cells_and_serde_roundtrips() {
        let mut stats = RouterStats::default();
        let k = (TaskKind::Implement, AgentType::ClaudeCode, "sonnet");
        assert_eq!(stats.cell(k.0, k.1, k.2), None);
        stats.record(k.0, k.1, k.2, Signal::Completed);
        stats.record(k.0, k.1, k.2, Signal::Failed);
        stats.record(k.0, k.1, k.2, Signal::RatedGood);
        stats.record(k.0, k.1, k.2, Signal::RatedBad);
        let cell = stats.cell(k.0, k.1, k.2).unwrap();
        approx(cell.good, 0.5 + 1.0);
        approx(cell.bad, 1.0 + 1.5);
        // Two tasks ended (completed, failed); ratings don't add tasks.
        assert_eq!(cell.n, 2);
        assert!(stats.cells.contains_key("implement|claude_code|sonnet"));

        stats.record(
            TaskKind::Shell,
            AgentType::OpenCode,
            "free:space-bunny-free",
            Signal::RatedGood,
        );
        assert!(stats
            .cells
            .contains_key("shell|open_code|free:space-bunny-free"));

        let json = serde_json::to_string(&stats).unwrap();
        let back: RouterStats = serde_json::from_str(&json).unwrap();
        assert_eq!(back, stats);

        // Missing fields default, so older stored blobs keep loading.
        let sparse: RouterStats =
            serde_json::from_str(r#"{"cells":{"plan|codex|opus":{"good":2.0}}}"#).unwrap();
        assert_eq!(
            sparse.cell(TaskKind::Plan, AgentType::Codex, "opus"),
            Some(&Cell {
                good: 2.0,
                bad: 0.0,
                n: 0
            })
        );
        assert_eq!(
            serde_json::from_str::<RouterStats>("{}").unwrap(),
            RouterStats::default()
        );
    }

    #[test]
    fn scorecard_hint_adjusts_quality_only_with_enough_samples() {
        let candidates = [cc("sonnet")];
        let hint = |category: &str, error_pct: f64, sample: u64| ScorecardHint {
            agent_type: AgentType::ClaudeCode,
            family: "sonnet".to_string(),
            category: category.to_string(),
            error_pct,
            sample,
        };
        let quality_with = |kind: TaskKind, hints: &[ScorecardHint]| {
            route(
                &input(kind, Difficulty::Normal),
                &candidates,
                &RouterStats::default(),
                hints,
                Profile::Calidad,
                &mut || 0.5,
            )
            .unwrap()
            .quality
        };
        // implement is "edit": 0.86 − 0.5 × 0.40 = 0.66.
        approx(quality_with(TaskKind::Implement, &[hint("edit", 40.0, 25)]), 0.66);
        approx(quality_with(TaskKind::Debug, &[hint("edit", 40.0, 20)]), 0.66);
        // Too few samples, wrong category, or no category: no adjustment.
        approx(quality_with(TaskKind::Implement, &[hint("edit", 40.0, 19)]), 0.86);
        approx(quality_with(TaskKind::Implement, &[hint("shell", 40.0, 99)]), 0.86);
        approx(quality_with(TaskKind::Plan, &[hint("edit", 40.0, 99)]), 0.76);
        approx(quality_with(TaskKind::Docs, &[hint("edit", 40.0, 99)]), 0.84);
        // Other mappings: review→explore, shell→shell, research→research.
        approx(quality_with(TaskKind::Review, &[hint("explore", 10.0, 30)]), 0.76 - 0.05);
        approx(quality_with(TaskKind::Shell, &[hint("shell", 20.0, 30)]), 0.84 - 0.10);
        approx(quality_with(TaskKind::Research, &[hint("research", 20.0, 30)]), 0.84 - 0.10);
        // A hint for another family or agent does not apply.
        let other_family = ScorecardHint {
            family: "opus".to_string(),
            ..hint("edit", 40.0, 99)
        };
        let other_agent = ScorecardHint {
            agent_type: AgentType::OpenCode,
            ..hint("edit", 40.0, 99)
        };
        approx(quality_with(TaskKind::Implement, &[other_family, other_agent]), 0.86);
    }

    #[test]
    fn quality_is_clamped_to_unit_interval() {
        let candidates = [cc("sonnet")];
        let mut stats = RouterStats::default();
        for _ in 0..20 {
            stats.record(TaskKind::Implement, AgentType::ClaudeCode, "sonnet", Signal::RatedBad);
        }
        let hints = [ScorecardHint {
            agent_type: AgentType::ClaudeCode,
            family: "sonnet".to_string(),
            category: "edit".to_string(),
            error_pct: 100.0,
            sample: 100,
        }];
        // 0.86·8 / (8 + 30) ≈ 0.18, minus 0.5 → below zero → 0.
        let d = route(
            &input(TaskKind::Implement, Difficulty::Normal),
            &candidates,
            &stats,
            &hints,
            Profile::Calidad,
            &mut || 0.5,
        )
        .unwrap();
        assert_eq!(d.quality, 0.0);
    }

    #[test]
    fn unknown_family_uses_default_prior_cost_and_latency() {
        let d = pick(
            TaskKind::Implement,
            Difficulty::Normal,
            &[cand(AgentType::OpenCode, "opencode/big-pickle")],
            &RouterStats::default(),
            Profile::Calidad,
        );
        approx(d.quality, 0.6);
        // implement/normal → medium (×0.8): cost 0.5 × 0.8, latency 0.5.
        approx(d.score, 0.8 * 0.6 - 0.12 * (0.5 * 0.8) - 0.08 * 0.5);
        assert_eq!(d.family, "opencode/big-pickle");
    }

    #[test]
    fn equilibrado_prefers_cheaper_when_qualities_are_close() {
        // plan/trivial → effort medium (×0.8).
        let candidates = [cc("opus"), cc("sonnet")];
        let stats = RouterStats::default();
        let calidad = pick(TaskKind::Plan, Difficulty::Trivial, &candidates, &stats, Profile::Calidad);
        let equilibrado = pick(
            TaskKind::Plan,
            Difficulty::Trivial,
            &candidates,
            &stats,
            Profile::Equilibrado,
        );
        assert_eq!(calidad.model, "opus");
        approx(calidad.score, 0.8 * 0.95 - 0.12 * (0.7 * 0.8) - 0.08 * 0.7);
        assert_eq!(equilibrado.model, "sonnet");
        approx(
            equilibrado.score,
            0.5 * 0.76 - 0.35 * (0.4 * 0.8) - 0.15 * 0.4,
        );
        assert_eq!(equilibrado.profile, Profile::Equilibrado);
        assert!(equilibrado.reason.contains("profile equilibrado"));
    }

    #[test]
    fn ties_break_by_agent_wire_then_model_regardless_of_input_order() {
        let a = cand(AgentType::ClaudeCode, "claude-sonnet-4");
        let b = cand(AgentType::ClaudeCode, "claude-sonnet-5");
        let c = cand(AgentType::OpenCode, "opencode/sonnet-x");
        let stats = RouterStats::default();
        let orders = [
            [a.clone(), b.clone(), c.clone()],
            [c.clone(), b.clone(), a.clone()],
            [b.clone(), c.clone(), a.clone()],
        ];
        for candidates in &orders {
            let d = pick(
                TaskKind::Implement,
                Difficulty::Hard,
                candidates,
                &stats,
                Profile::Calidad,
            );
            assert_eq!(d.model, "claude-sonnet-4");
            assert_eq!(
                ranked(&d),
                [
                    "claude_code/claude-sonnet-4",
                    "claude_code/claude-sonnet-5",
                    "open_code/opencode/sonnet-x"
                ]
            );
        }
    }
}
