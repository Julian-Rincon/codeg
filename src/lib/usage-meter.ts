/**
 * Token-usage meter: the Claude plan quota (5 h / week) plus Phantom's own
 * per-agent/model token tally for today, and the automatic quota-resume
 * scheduler.
 *
 * Mirrors the backend's documented contract:
 * - `usage_meter` `{}` → the plan quota + today's tokens (Claude cached 60 s
 *   server-side). Read-only.
 * - `quota_resume_list` `{}`, `quota_resume_schedule` `{connection_id,
 *   conversation_id, agent_type}`, `quota_resume_cancel` `{id}`.
 *
 * The calls go through the transport exactly like `lib/model-scorecard.ts`'s
 * `fetchModelScorecard`. Every field the backend may omit is typed
 * nullable/optional so a server that predates it degrades to "nothing to
 * show" rather than a runtime crash.
 */

import { getTransport } from "./transport"
import type { AgentType } from "./types"

// ── Wire contract ────────────────────────────────────────────────────────

/** One quota window (`five_hour` / `seven_day`): `percent` is 0–100
 *  utilization, `resets_at` the ISO timestamp the window rolls over. */
export interface UsageMeterWindow {
  percent: number
  resets_at: string | null
}

/** The Claude plan's live quota. `null` (on the response) when the backend
 *  could not read the OAuth credentials — see `claude_error`. */
export interface UsageMeterClaude {
  /** Plan name, e.g. `pro`. */
  plan: string | null
  five_hour: UsageMeterWindow
  seven_day: UsageMeterWindow
  /** When the backend last read the quota (ISO). */
  fetched_at: string | null
}

/** One agent/model's recorded turn tokens for the current day. */
export interface UsageMeterTodayEntry {
  agent_type: AgentType
  model: string
  input_tokens: number
  output_tokens: number
  turns: number
}

export interface UsageMeterResponse {
  /** `null` when the quota could not be read (`claude_error` says why). */
  claude: UsageMeterClaude | null
  /** Why `claude` is `null` / stale, or `null`. */
  claude_error: string | null
  /** Today's measured tokens, one row per agent/model. */
  today: UsageMeterTodayEntry[]
  /** The rolling week's total token count. */
  week_tokens: number
}

/** A scheduled automatic resume of a quota-limited conversation. */
export interface QuotaResume {
  id: string | number
  conversation_id: number
  agent_type: AgentType
  /** ISO timestamp the conversation will be resumed at. */
  resume_at: string
  status: string
  attempts: number
}

export interface ScheduleQuotaResumeInput {
  connection_id: string
  conversation_id: number
  agent_type: AgentType
}

export interface ScheduledQuotaResume {
  id: string | number
  /** ISO timestamp the conversation will be resumed at. */
  resume_at: string
}

// ── Fetch ────────────────────────────────────────────────────────────────

/**
 * Fetch the usage meter. Returns `null` on ANY failure — a 404 from a server
 * that predates this endpoint, a network error, a malformed body — so callers
 * degrade to showing nothing. Never rejects (mirrors `fetchModelScorecard`).
 */
export async function fetchUsageMeter(): Promise<UsageMeterResponse | null> {
  try {
    return await getTransport().call<UsageMeterResponse>("usage_meter", {})
  } catch {
    return null
  }
}

/**
 * List the scheduled quota resumes. Returns `null` on ANY failure (old
 * server, network error) so callers degrade to "nothing scheduled".
 */
export async function fetchQuotaResumes(): Promise<QuotaResume[] | null> {
  try {
    return await getTransport().call<QuotaResume[]>("quota_resume_list", {})
  } catch {
    return null
  }
}

/**
 * Schedule an automatic resume for `connection_id`'s conversation. Unlike
 * {@link fetchUsageMeter} this REJECTS on failure: the user just asked for
 * it, so the caller surfaces the error inline instead of silently doing
 * nothing (mirrors `postPhantomHandoff`).
 */
export async function scheduleQuotaResume(
  input: ScheduleQuotaResumeInput
): Promise<ScheduledQuotaResume> {
  return getTransport().call<ScheduledQuotaResume>("quota_resume_schedule", {
    connection_id: input.connection_id,
    conversation_id: input.conversation_id,
    agent_type: input.agent_type,
  })
}

/** Cancel a scheduled resume. Rejects on failure, like the schedule above. */
export async function cancelQuotaResume(id: string | number): Promise<void> {
  await getTransport().call<{ ok: boolean }>("quota_resume_cancel", { id })
}

// ── Pure helpers ─────────────────────────────────────────────────────────

export type UsageLevel = "ok" | "warn" | "high"

/**
 * Severity bucket for a utilization percentage: green below 70, amber below
 * 90, red at or above 90. Non-finite input reads as `ok` — an unreadable
 * number is not evidence of a limit.
 */
export function usageLevel(percent: number): UsageLevel {
  if (!Number.isFinite(percent) || percent < 70) return "ok"
  if (percent < 90) return "warn"
  return "high"
}

/**
 * Compact token count: `1.2k`, `3.4M`, or the plain number under a thousand.
 * Unit-bare and locale-independent — the abbreviation reads the same in
 * every catalog (matching `formatScorecardContext`).
 */
export function formatTokens(n: number): string {
  if (!Number.isFinite(n) || n <= 0) return "0"
  if (n >= 1_000_000) return `${oneDecimal(n / 1_000_000)}M`
  if (n >= 1_000) return `${oneDecimal(n / 1_000)}k`
  return String(Math.round(n))
}

function oneDecimal(value: number): string {
  const rounded = Math.round(value * 10) / 10
  return Number.isInteger(rounded) ? String(rounded) : rounded.toFixed(1)
}
