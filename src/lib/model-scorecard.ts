"use client"

import { useEffect } from "react"
import { create } from "zustand"
import { getTransport } from "./transport"
import type { AgentType } from "./types"

// ── Wire contract ────────────────────────────────────────────────────────
// Mirrors the backend's `POST /api/model_scorecard` response (snake_case,
// matching every other endpoint in `types.ts`). Measured-only: every numeric
// field is nullable because a model with zero recorded turns has nothing to
// report, and the UI must never synthesize a number to fill the gap.

export type ModelScorecardCategory =
  | "edit"
  | "explore"
  | "shell"
  | "research"
  | "fast"
  | "long_context"
  | "free"

export interface ModelScorecardSpec {
  context: number | null
  reasoning: boolean | null
  tool_call: boolean | null
  cost_in: number | null
  cost_out: number | null
}

export interface ModelScorecardCategoryCalls {
  edit: number
  read: number
  shell: number
  web: number
  agent: number
}

export interface ModelScorecardCategoryErrorPct {
  edit: number | null
  read: number | null
  shell: number | null
  web: number | null
  agent: number | null
}

export interface ModelScorecardEntry {
  agent_type: AgentType
  model: string
  label: string | null
  available: boolean | null
  conversations: number
  turns: number
  avg_turn_ms: number | null
  p50_turn_ms: number | null
  output_tokens_per_s: number | null
  output_tokens_per_turn: number | null
  cache_hit_pct: number | null
  tool_calls: number
  tool_error_pct: number | null
  category_calls: ModelScorecardCategoryCalls
  category_error_pct: ModelScorecardCategoryErrorPct
  last_used_at: string | null
  spec: ModelScorecardSpec | null
  /** Categories this model measurably wins at (subset of `best_for`'s
   *  categories) — the badge source for the model picker and mention hint. */
  strengths: ModelScorecardCategory[]
  /** Too few turns to trust the averages above — the UI must show "not
   *  enough data" instead of any of them, never a number computed on ~1 turn. */
  low_sample: boolean
  /** True when the backend currently sees this agent/model as quota/rate
   *  limited (mirrors `POST /api/phantom_successor`'s `limited` signal onto
   *  every scorecard row for that model). Optional — a server that predates
   *  this field omits it, treated as `false`. Drives the model picker's muted
   *  "Out of tokens" chip; the model stays selectable either way. */
  limited?: boolean
  /** ISO timestamp the limit is expected to clear, or `null`/absent when
   *  there's no estimate. Optional for the same reason as `limited`. */
  limit_resets_at?: string | null
}

export interface ModelScorecardBestForRunnerUp {
  agent_type: AgentType
  model: string
  value: number
}

export interface ModelScorecardBestFor {
  category: ModelScorecardCategory
  agent_type: AgentType
  model: string
  metric: "error_pct" | "output_tokens_per_s" | "context"
  value: number
  sample: number
  runner_up: ModelScorecardBestForRunnerUp | null
}

export interface ModelScorecardResponse {
  generated_at: string
  models: ModelScorecardEntry[]
  best_for: ModelScorecardBestFor[]
}

// ── Fetch ────────────────────────────────────────────────────────────────

/**
 * Fetch the scorecard. Returns `null` on ANY failure — a 404 from a server
 * that predates this endpoint, a network error, a malformed body — so every
 * caller can treat "no data" and "old server" identically and degrade to
 * showing nothing rather than throwing. Never rejects.
 */
export async function fetchModelScorecard(): Promise<ModelScorecardResponse | null> {
  try {
    return await getTransport().call<ModelScorecardResponse>(
      "model_scorecard",
      {}
    )
  } catch {
    return null
  }
}

// ── Matcher ──────────────────────────────────────────────────────────────

/**
 * Resolve an ACP session-config model option back to its scorecard entry.
 *
 * The scorecard's `model` is always the bare id the backend measured turns
 * against (`claude-sonnet-5`), while an option's `value` may carry a
 * `provider/` prefix (`opencode/nemotron-3.5-lightning-free`) or be an agent
 * alias the scorecard never sees (`default`, `opus`, `sonnet` — those
 * legitimately match nothing and the caller shows "no data" for them, never a
 * guessed number). Tries, per `agent_type`, in order:
 *   1. exact id match
 *   2. the value with its leading `provider/` segment stripped
 *   3. the value's last `/`-separated segment
 *   4. the option's display label against the entry's label or id
 * Returns the first hit, or `null` when nothing matches.
 */
export function findModelScore(
  scorecard: ModelScorecardResponse | null,
  agentType: AgentType,
  optionValue: string,
  optionLabel?: string | null
): ModelScorecardEntry | null {
  if (!scorecard || !optionValue) return null
  const candidates = scorecard.models.filter(
    (entry) => entry.agent_type === agentType
  )
  if (candidates.length === 0) return null

  const normalize = (s: string) => s.trim().toLowerCase()
  const value = normalize(optionValue)
  const slashIndex = optionValue.indexOf("/")
  const prefixStripped =
    slashIndex >= 0 && slashIndex < optionValue.length - 1
      ? normalize(optionValue.slice(slashIndex + 1))
      : null
  const lastSegment = normalize(optionValue.split("/").pop() || optionValue)
  const label = optionLabel?.trim() ? normalize(optionLabel) : null
  // Alias ids (`sonnet`, `opus`, `default`) only carry the family in the
  // label: "Opus 5.5" -> `opus-5-5`, which transcripts record as
  // `claude-opus-5-5`. The slug must match a whole trailing segment so
  // `opus-5` never claims `claude-opus-5-5`.
  const slug = label
    ? label.replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "")
    : ""

  const rules: Array<(entry: ModelScorecardEntry) => boolean> = [
    (e) => normalize(e.model) === value,
    (e) => prefixStripped !== null && normalize(e.model) === prefixStripped,
    (e) => lastSegment !== value && normalize(e.model) === lastSegment,
    (e) =>
      label !== null &&
      ((!!e.label && normalize(e.label) === label) ||
        normalize(e.model) === label),
    (e) => {
      if (!slug) return false
      const id = normalize(e.model.split("/").pop() || e.model)
      return id === slug || id.endsWith(`-${slug}`)
    },
  ]
  const matches: ModelScorecardEntry[] = []
  for (const rule of rules) {
    for (const entry of candidates) {
      if (rule(entry) && !matches.includes(entry)) matches.push(entry)
    }
  }
  // The live catalog records alias ids with zero usage (`sonnet`), while the
  // transcripts record the resolved id (`claude-sonnet-5`). Prefer the match
  // that carries measurements; fall back to the best-ranked match so an
  // unused model still reports its availability.
  return matches.find(hasScorecardData) ?? matches[0] ?? null
}

/** Whether an entry carries measured numbers worth showing (as opposed to a
 *  "not enough data" placeholder). Zero turns and `low_sample` both count as
 *  no data — the point of the flag is exactly to keep a ~1-turn average off
 *  the screen. */
export function hasScorecardData(
  entry: ModelScorecardEntry | null | undefined
): entry is ModelScorecardEntry {
  return !!entry && !entry.low_sample && entry.turns > 0
}

/** Whether the scorecard currently reports this model as quota/rate limited.
 *  `undefined`/missing (an older server) reads as not limited. */
export function isModelLimited(
  entry: ModelScorecardEntry | null | undefined
): boolean {
  return entry?.limited === true
}

/** Short locale-aware clock time for a `limit_resets_at` ISO timestamp
 *  (e.g. "5:00 AM" / "05:00"), or `null` when it's missing/unparseable — the
 *  caller falls back to a generic label with no time. */
export function formatLimitResetTime(
  isoTimestamp: string | null | undefined,
  locale?: string
): string | null {
  if (!isoTimestamp) return null
  const date = new Date(isoTimestamp)
  if (Number.isNaN(date.getTime())) return null
  return new Intl.DateTimeFormat(locale ?? "en-US", {
    hour: "numeric",
    minute: "2-digit",
  }).format(date)
}

// ── Formatters ───────────────────────────────────────────────────────────
// Locale-aware but unit-bare (the caller supplies the unit/word via i18n) —
// see `buildScorecardMetricLine` below for the composed line these feed.

function localeRound(value: number, locale?: string): string {
  const digits = Math.abs(value) < 10 ? 1 : 0
  // Default to "en-US" rather than the runtime's own default locale (which,
  // server-side, is whatever the host OS is set to and has nothing to do
  // with the viewer) — callers pass the real UI locale explicitly.
  return new Intl.NumberFormat(locale ?? "en-US", {
    minimumFractionDigits: digits,
    maximumFractionDigits: digits,
  }).format(value)
}

/** Average turn duration in whole/tenths of a second, e.g. `14` or `0.9`. */
export function formatScorecardSeconds(
  ms: number | null | undefined,
  locale?: string
): string | null {
  if (ms == null || !Number.isFinite(ms) || ms < 0) return null
  return localeRound(ms / 1000, locale)
}

/** Output tokens/second, e.g. `56` or `4.3`. */
export function formatScorecardTokensPerSecond(
  value: number | null | undefined,
  locale?: string
): string | null {
  if (value == null || !Number.isFinite(value) || value < 0) return null
  return localeRound(value, locale)
}

/** A percentage value (error rate, cache hit rate), e.g. `3` or `1.2`. */
export function formatScorecardPercent(
  value: number | null | undefined,
  locale?: string
): string | null {
  if (value == null || !Number.isFinite(value) || value < 0) return null
  return localeRound(value, locale)
}

/** Context window, compacted like `262k` / `1M` / `8k`. Never localized —
 *  the abbreviation reads the same in every catalog (see the product spec's
 *  own es example, `262k ctx`, which leaves it untranslated). */
export function formatScorecardContext(
  tokens: number | null | undefined
): string | null {
  if (tokens == null || !Number.isFinite(tokens) || tokens <= 0) return null
  if (tokens >= 1_000_000) {
    const millions = tokens / 1_000_000
    return `${Number.isInteger(millions) ? millions : millions.toFixed(1)}M`
  }
  if (tokens >= 1_000) {
    return `${Math.round(tokens / 1000)}k`
  }
  return String(Math.round(tokens))
}

/** Minimal shape of `useTranslations()` this module needs — kept structural
 *  so the pure formatter below is testable without next-intl. */
export type ScorecardTranslate = (
  key:
    | "metricDuration"
    | "metricTokPerSec"
    | "metricErrorPct"
    | "metricContext",
  values: { value: string }
) => string

/** Broader structural shape for the helpers below, which also resolve
 *  category labels and the "no data" string — still no next-intl import
 *  needed for tests, and a real `useTranslations("ModelScorecard")` return
 *  value satisfies it as-is. */
export type AnyTranslate = (
  key: string,
  values?: Record<string, string | number>
) => string

/** `ModelScorecardCategory` → its key under the `ModelScorecard` i18n
 *  namespace. Shared by the model picker badges, the mention hint, and the
 *  Modelos table so every surface uses the same short label. */
export const SCORECARD_CATEGORY_LABEL_KEYS: Record<
  ModelScorecardCategory,
  string
> = {
  edit: "categoryEdit",
  explore: "categoryExplore",
  shell: "categoryShell",
  research: "categoryResearch",
  fast: "categoryFast",
  long_context: "categoryLongContext",
  free: "categoryFree",
}

/** Localized short chips for the categories `entry` measurably wins at
 *  (`entry.strengths`) — the model picker / mention hint badge source. */
export function scorecardStrengthLabels(
  entry: Pick<ModelScorecardEntry, "strengths">,
  t: AnyTranslate
): string[] {
  return entry.strengths.map((category) =>
    t(SCORECARD_CATEGORY_LABEL_KEYS[category])
  )
}

/**
 * The muted status line for a model row: the measured metric summary when
 * there's enough data, else the localized "not enough data yet" note —
 * never a number computed from too few turns. Always returns a string (as
 * opposed to {@link buildScorecardMetricLine}, which can return `null`).
 */
export function scorecardStatusLine(
  entry: ModelScorecardEntry,
  t: AnyTranslate,
  locale?: string
): string {
  if (!hasScorecardData(entry)) return t("noData")
  const line = buildScorecardMetricLine(
    entry,
    t as unknown as ScorecardTranslate,
    locale
  )
  return line ?? t("noData")
}

/**
 * The compact muted metric line shown under a model option / table row, e.g.
 * "14 s/turn · 56 tok/s · 3% errors · 262k ctx". Each segment is included
 * only when the underlying measurement exists — a model with no context spec
 * simply has a shorter line, never a placeholder dash. Returns `null` when
 * nothing at all is available (the caller falls back to a "not enough data"
 * label via {@link hasScorecardData}).
 */
export function buildScorecardMetricLine(
  entry: ModelScorecardEntry,
  t: ScorecardTranslate,
  locale?: string
): string | null {
  const parts: string[] = []
  const duration = formatScorecardSeconds(entry.avg_turn_ms, locale)
  if (duration) parts.push(t("metricDuration", { value: duration }))
  const tps = formatScorecardTokensPerSecond(entry.output_tokens_per_s, locale)
  if (tps) parts.push(t("metricTokPerSec", { value: tps }))
  const errorPct = formatScorecardPercent(entry.tool_error_pct, locale)
  if (errorPct) parts.push(t("metricErrorPct", { value: errorPct }))
  const context = formatScorecardContext(entry.spec?.context ?? null)
  if (context) parts.push(t("metricContext", { value: context }))
  return parts.length > 0 ? parts.join(" · ") : null
}

// ── Delegation (`@`-mention) hint ───────────────────────────────────────

/** The formatted value + unit for one `best_for` win, e.g. `"1.2% errors"`,
 *  `"56 tok/s"`, `"262k ctx"` — dispatches on `metric` since each category
 *  ranks by a different measurement. */
export function formatBestForMetric(
  bestFor: Pick<ModelScorecardBestFor, "metric" | "value">,
  t: AnyTranslate,
  locale?: string
): string | null {
  switch (bestFor.metric) {
    case "error_pct": {
      const value = formatScorecardPercent(bestFor.value, locale)
      return value ? t("metricErrorPct", { value }) : null
    }
    case "output_tokens_per_s": {
      const value = formatScorecardTokensPerSecond(bestFor.value, locale)
      return value ? t("metricTokPerSec", { value }) : null
    }
    case "context": {
      const value = formatScorecardContext(bestFor.value)
      return value ? t("metricContext", { value }) : null
    }
    default:
      return null
  }
}

/**
 * One localized hint per agent type that has at least one `best_for` win,
 * e.g. "Claude Code · Opus 5.5 — best at Edit (1.2% errors, n=340)" — the
 * `@`-mention panel's delegation hint. Picks the win with the largest
 * `sample` when an agent tops more than one category (the most
 * statistically confident claim to lead with); this is a hint only, never
 * wired to change what gets delegated.
 *
 * `agentLabels` resolves each `best_for.agent_type` to its display name
 * (the mention panel already has one per agent via `getAgentLabel`/the
 * agent's own `name` — reused here instead of guessing a second one).
 */
export function buildAgentMentionHints(
  scorecard: ModelScorecardResponse | null,
  agentLabels: Map<AgentType, string>,
  t: AnyTranslate,
  locale?: string
): Map<AgentType, string> {
  const hints = new Map<AgentType, string>()
  if (!scorecard) return hints

  const bestByAgent = new Map<AgentType, ModelScorecardBestFor>()
  for (const bestFor of scorecard.best_for) {
    const current = bestByAgent.get(bestFor.agent_type)
    if (!current || bestFor.sample > current.sample) {
      bestByAgent.set(bestFor.agent_type, bestFor)
    }
  }

  for (const [agentType, bestFor] of bestByAgent) {
    const metric = formatBestForMetric(bestFor, t, locale)
    if (!metric) continue
    const modelEntry = scorecard.models.find(
      (m) => m.agent_type === agentType && m.model === bestFor.model
    )
    const modelLabel = modelEntry?.label || bestFor.model
    const agentLabel = agentLabels.get(agentType) ?? agentType
    const category = t(SCORECARD_CATEGORY_LABEL_KEYS[bestFor.category])
    hints.set(
      agentType,
      t("mentionHint", {
        agent: agentLabel,
        model: modelLabel,
        category,
        metric,
        sample: bestFor.sample,
      })
    )
  }
  return hints
}

// ── Client cache hook ────────────────────────────────────────────────────

interface ModelScorecardStore {
  scorecard: ModelScorecardResponse | null
  loading: boolean
  reload: () => Promise<void>
}

// Monotonic request id so a slow, superseded fetch can never clobber a
// faster later one (same guard shape as `use-acp-agents.ts`).
let latestRequestId = 0

// One fetch in flight at a time: every picker, selector and mention menu
// mounts this hook, and a focus event would otherwise fan out into one
// request per mounted consumer.
let inflight: Promise<void> | null = null

const useModelScorecardStore = create<ModelScorecardStore>((set) => ({
  scorecard: null,
  loading: false,
  reload: () => {
    if (inflight) return inflight
    const requestId = ++latestRequestId
    set({ loading: true })
    inflight = fetchModelScorecard()
      .then((result) => {
        if (requestId === latestRequestId) {
          set({ scorecard: result, loading: false })
        }
      })
      .finally(() => {
        inflight = null
      })
    return inflight
  },
}))

// Refresh timer and focus listener are shared across every mounted consumer
// and armed only while at least one is mounted.
let subscribers = 0
let stopPolling: (() => void) | null = null

function startPolling(reload: () => Promise<void>): () => void {
  const interval = setInterval(() => void reload(), REFRESH_INTERVAL_MS)
  const onFocus = () => void reload()
  window.addEventListener("focus", onFocus)
  return () => {
    clearInterval(interval)
    window.removeEventListener("focus", onFocus)
  }
}

const REFRESH_INTERVAL_MS = 5 * 60 * 1000

export interface UseModelScorecardResult {
  /** `null` until the first fetch resolves, or when the endpoint is missing
   *  (old server) / errored — every consumer treats those identically. */
  scorecard: ModelScorecardResponse | null
  loading: boolean
  /** Manual refresh — wired to the "Sincronizar" action on the Modelos tab. */
  refresh: () => Promise<void>
}

/**
 * Shared, module-level scorecard cache. The first mount across the window
 * triggers the initial fetch (later mounts just read the cached value —
 * "fetch once per window"); every mount also arms a 5-minute refresh timer
 * and a window-focus refresh, both idempotent against the request-id guard
 * above. Degrades to `scorecard: null` with no throw when the backend
 * doesn't implement the endpoint yet.
 */
export function useModelScorecard(): UseModelScorecardResult {
  const scorecard = useModelScorecardStore((s) => s.scorecard)
  const loading = useModelScorecardStore((s) => s.loading)
  const reload = useModelScorecardStore((s) => s.reload)

  useEffect(() => {
    if (useModelScorecardStore.getState().scorecard === null) {
      void reload()
    }
    subscribers += 1
    if (subscribers === 1) stopPolling = startPolling(reload)
    return () => {
      subscribers -= 1
      if (subscribers === 0) {
        stopPolling?.()
        stopPolling = null
      }
    }
  }, [reload])

  return { scorecard, loading, refresh: reload }
}
