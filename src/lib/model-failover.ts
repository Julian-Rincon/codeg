"use client"

/**
 * Model quota failover: when the active agent/model runs out of tokens
 * mid-conversation, offer the best MEASURED successor (from the scorecard)
 * and, on acceptance, hand the conversation over with full context.
 *
 * Three pieces:
 * - the wire contract for `POST /api/phantom_successor` / `POST
 *   /api/phantom_handoff` (backend-owned; called the same way every other
 *   endpoint is — see `lib/model-scorecard.ts`'s `fetchModelScorecard` for
 *   the sibling pattern this mirrors);
 * - the limit matcher: is a given AIR session-failure record, or a plain turn-
 *   error string, a "ran out of tokens/quota" event;
 * - `useFailoverOffer`: given a live limit trigger for one conversation,
 *   fetches the successor once (deduped across concurrent mounts) and drives
 *   the handoff card's accept/dismiss/retry lifecycle.
 */

import { useCallback, useEffect, useState } from "react"
import { getTransport } from "./transport"
import { toErrorMessage } from "./app-error"
import type { AgentType, SessionFailureRecord } from "@/lib/types"

// ── Wire contract ────────────────────────────────────────────────────────
// `POST /api/phantom_successor` and `POST /api/phantom_handoff` — snake_case
// response bodies (matching every other endpoint in `types.ts`), camelCase
// request bodies (as specified by the backend contract this was built
// against). Every field the backend may omit is typed nullable/optional so a
// server that predates a field degrades to "nothing to show" rather than a
// runtime crash.

/** What the successor lookup found was actually limiting the CURRENT agent —
 *  `null` when the backend has no record of a limit (a stale/late call, or a
 *  server too old to track it). */
export interface PhantomLimitedInfo {
  agent_type: AgentType
  /** `"account"` — every model on this agent is blocked; `"model"` — only
   *  the one named in `model`. */
  scope: "account" | "model"
  /** Omitted when the hit wasn't tied to one specific model. */
  model?: string | null
  /** Adapter/backend-authored wording, e.g. "You've hit your session limit". */
  message: string
  hit_at: string
  /** Human phrasing of the reset, e.g. "resets 5am" — shown verbatim when
   *  present; `resets_at` is the machine-readable fallback. Both omitted when
   *  no reset could be parsed out of the failure text. */
  resets_hint?: string | null
  resets_at?: string | null
}

export interface PhantomSuccessorMetrics {
  turns: number
  tool_error_pct?: number | null
  output_tokens_per_s?: number | null
  context?: number | null
}

export interface PhantomSuccessorCandidate {
  agent_type: AgentType
  model: string
  label?: string | null
  /** The short measured claim to show next to the chip, e.g. "0.4% tool
   *  errors, 62 tok/s". */
  reason: string
  metrics: PhantomSuccessorMetrics
}

export interface PhantomSuccessorResponse {
  limited: PhantomLimitedInfo | null
  successor: PhantomSuccessorCandidate | null
  runner_up: PhantomSuccessorCandidate | null
}

export interface PhantomHandoffResponse {
  conversation_id: number
  folder_id: number
  agent_type: AgentType
  model?: string | null
  connection_id: string
}

/**
 * Fetch the measured successor for `agentType`/`model` on `conversationId`.
 * Returns `null` on ANY failure — a 404 from a server that predates this
 * endpoint, a network error, a malformed body — so callers degrade to "no
 * offer" identically to an old server and a genuinely empty answer. Never
 * rejects (mirrors `fetchModelScorecard`).
 */
export async function fetchPhantomSuccessor(
  agentType: AgentType,
  model: string | null,
  conversationId: number | null
): Promise<PhantomSuccessorResponse | null> {
  try {
    return await getTransport().call<PhantomSuccessorResponse>(
      "phantom_successor",
      { agentType, model, conversationId }
    )
  } catch {
    return null
  }
}

/**
 * Hand a conversation off to a successor agent/model — the backend transfers
 * full context and returns where it landed. Unlike the fetch above this
 * REJECTS on failure: the user just asked for this to happen, so the card
 * surfaces the error inline with a retry rather than silently doing nothing.
 */
export async function postPhantomHandoff(
  conversationId: number,
  targetAgentType: AgentType,
  targetModel: string | null
): Promise<PhantomHandoffResponse> {
  return getTransport().call<PhantomHandoffResponse>("phantom_handoff", {
    conversationId,
    targetAgentType,
    targetModel,
  })
}

// ── Limit matcher ────────────────────────────────────────────────────────
// Mirrors the backend's own heuristic (`acp::model_limits::{QUOTA_PATTERNS,
// EXHAUSTED_PATTERNS}`) exactly — same substrings, same two-tier rule — so
// the card offers a handoff for precisely the hits the backend itself will
// later report through `model_scorecard`'s `limited` flag and
// `phantom_successor`'s `limited` field. Kept as flat substring lists rather
// than regexes: this is adapter-authored free text from several vendors, so
// a case-insensitive substring match is both the most robust option and the
// easiest to extend.

/** Case-insensitive substrings that mean "this is a quota/rate exhaustion,
 *  not an ordinary failure" — applied to a session-failure's title+details or
 *  a turn error's plain text. */
const QUOTA_PATTERNS: string[] = [
  "session limit",
  "usage limit",
  "rate limit",
  "rate_limit",
  "quota",
  "insufficient_quota",
  "out of credits",
  "credit balance",
  "spend limit",
  "exceeded your",
  "too many requests",
  "429",
]

/** Narrower subset used to promote a `severity: "warning"` record to a real
 *  hit — a warning-level AIR record is usually a transient, auto-recovering
 *  condition, so only text that says the quota is actually SPENT (not merely
 *  throttled) promotes it; "rate limit" / "too many requests" / "429" stay
 *  error-severity-only. */
const EXHAUSTED_PATTERNS: string[] = [
  "session limit",
  "usage limit",
  "quota",
  "insufficient_quota",
  "out of credits",
  "credit balance",
  "spend limit",
  "exceeded your",
]

function containsAny(text: string, needles: string[]): boolean {
  const lower = text.toLowerCase()
  return needles.some((needle) => lower.includes(needle))
}

/** Whether free text (a turn error, a session-failure's title/details) reads
 *  as a usage/quota/rate-limit failure. */
export function matchesLimitText(text: string | null | undefined): boolean {
  if (!text) return false
  return containsAny(text, QUOTA_PATTERNS)
}

/**
 * Whether an AIR session-failure record is the "ran out of tokens" trigger.
 * `category === "limit"` is the strong signal, but a `severity: "warning"`
 * record under it still needs the NARROWER exhausted-quota wording to
 * promote (a warning is usually the adapter reconnecting on its own — see
 * `lib/session-failures.ts`'s `isRetryIncident`); an unresolved
 * `error`-severity record under any OTHER category catches the rest (a
 * blank/other category, or codeg's own verdict mirrored into a synthetic
 * record — see `sessionFailureNotice`'s doc on that mirroring).
 */
export function isLimitSessionFailure(failure: SessionFailureRecord): boolean {
  if (failure.resolved) return false
  const combined = `${failure.title}\n${failure.details ?? ""}`
  if (failure.category === "limit") {
    return (
      failure.severity === "error" || containsAny(combined, EXHAUSTED_PATTERNS)
    )
  }
  if (failure.severity !== "error") return false
  return containsAny(combined, QUOTA_PATTERNS)
}

export interface ModelFailoverTrigger {
  /** Identifies WHICH hit this is — `<id>@<revision>` for a typed record, or
   *  the raw error text for a plain turn error. Used to detect a new/escalated
   *  hit (re-offer after a dismiss) vs. the one already on screen. */
  key: string
  /** User-facing wording to quote in the handoff card's title. */
  reasonText: string
}

/**
 * Scan a connection's session-failure table (latest first) and its last turn
 * error text for the limit trigger. Pass `useConnection(tabId).sessionFailures`
 * and `.error` straight through — this is intentionally not another
 * subscription onto the connections store (a `conversationId` doesn't map
 * 1:1 onto a live connection the way a `contextKey` does — cross-client
 * viewers and delegation children can share one), so the caller who already
 * holds the connection's live state is the one who calls this and passes
 * the result into {@link useFailoverOffer}.
 */
export function detectLimitTrigger(
  failures: SessionFailureRecord[] | null | undefined,
  lastErrorText?: string | null
): ModelFailoverTrigger | null {
  const list = failures ?? []
  for (let i = list.length - 1; i >= 0; i--) {
    const failure = list[i]
    if (isLimitSessionFailure(failure)) {
      const reasonText = failure.title.trim() || failure.details?.trim() || ""
      return { key: `${failure.id}@${failure.revision}`, reasonText }
    }
  }
  if (matchesLimitText(lastErrorText)) {
    return { key: `error:${lastErrorText}`, reasonText: lastErrorText!.trim() }
  }
  return null
}

// ── `useFailoverOffer` ──────────────────────────────────────────────────

/** conversationId → the in-flight successor fetch, so several mounts (a
 *  tiled duplicate view, a fast remount) never fan out into duplicate calls —
 *  same dedup shape as `model-scorecard.ts`'s single `inflight` slot, keyed
 *  per conversation since several conversations can be limited at once. */
const inflightByConversation = new Map<
  number,
  Promise<PhantomSuccessorResponse | null>
>()

function fetchDeduped(
  conversationId: number,
  agentType: AgentType,
  model: string | null
): Promise<PhantomSuccessorResponse | null> {
  const existing = inflightByConversation.get(conversationId)
  if (existing) return existing
  const promise = fetchPhantomSuccessor(agentType, model, conversationId)
  inflightByConversation.set(conversationId, promise)
  void promise.finally(() => {
    if (inflightByConversation.get(conversationId) === promise) {
      inflightByConversation.delete(conversationId)
    }
  })
  return promise
}

export interface UseFailoverOfferResult {
  /** The fetched successor/runner-up/limited info, or `null` while nothing is
   *  in flight or being shown (no trigger, dismissed, still loading, or the
   *  endpoint returned nothing — a 404 from an older server and a genuinely
   *  empty answer read identically, see {@link fetchPhantomSuccessor}). */
  offer: PhantomSuccessorResponse | null
  /** Accept the primary successor (default) or an explicit candidate (the
   *  "Elegir otro" runner-up). Resolves to where the handoff landed, or
   *  `null` on failure (also captured in `error`). */
  accept: (
    candidate?: Pick<PhantomSuccessorCandidate, "agent_type" | "model">
  ) => Promise<PhantomHandoffResponse | null>
  /** Close the card for the CURRENT trigger ("Esperar") — a later, distinct
   *  hit (new record revision, a new error) re-offers. */
  dismiss: () => void
  /** True while a handoff POST is in flight. */
  accepting: boolean
  /** The last `accept()` failure, localized-ready plain text (the successor
   *  lookup itself never rejects, so this is always a handoff error). */
  error: string | null
  /** Loading the successor lookup for the current trigger. */
  loading: boolean
  /** Re-run the successor lookup for the current trigger (e.g. it returned
   *  nothing the first time and the user wants a fresh read). */
  retry: () => void
}

interface FailoverResultEntry {
  offer: PhantomSuccessorResponse | null
}

/**
 * Fetches the measured successor once per distinct limit trigger for one
 * conversation, and drives the handoff card's lifecycle. `trigger` is
 * `null` outside a limit failure — nothing is shown or fetched. Pass it from
 * {@link detectLimitTrigger} over the conversation's live `sessionFailures`
 * + last turn-error text.
 *
 * Results are kept in `useState` (a `key → entry` map), written to ONLY from
 * inside the fetch's `.then()` — an async continuation, not the effect's own
 * synchronous execution. The repo's lint gate forbids both a `useState`
 * setter called synchronously from an effect body AND reading a `ref` during
 * render, so `loading` for a given trigger is deliberately not tracked as its
 * own flag anywhere — it's simply "this key has no entry in the results map
 * yet", derived at render time from state the render is already allowed to
 * read.
 */
export function useFailoverOffer(
  conversationId: number | null,
  agentType: AgentType | null,
  currentModel: string | null,
  trigger: ModelFailoverTrigger | null
): UseFailoverOfferResult {
  const [results, setResults] = useState<Map<string, FailoverResultEntry>>(
    () => new Map()
  )
  const [refreshTick, setRefreshTick] = useState(0)
  const [accepting, setAccepting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [dismissedKey, setDismissedKey] = useState<string | null>(null)

  const triggerKey = trigger?.key ?? null

  useEffect(() => {
    if (!triggerKey || triggerKey === dismissedKey) return
    if (conversationId == null || agentType == null) return
    let cancelled = false
    fetchDeduped(conversationId, agentType, currentModel).then((result) => {
      if (cancelled) return
      setResults((prev) => {
        const next = new Map(prev)
        next.set(triggerKey, { offer: result })
        return next
      })
    })
    return () => {
      cancelled = true
    }
    // `refreshTick` isn't read in the body — it only forces `retry()` to
    // re-run this effect after clearing the key's entry below (the trigger
    // key itself doesn't change on a retry, so nothing else here would).
  }, [
    triggerKey,
    dismissedKey,
    conversationId,
    agentType,
    currentModel,
    refreshTick,
  ])

  const activeKey =
    triggerKey && triggerKey !== dismissedKey ? triggerKey : null
  const offer = (activeKey ? results.get(activeKey)?.offer : null) ?? null
  const loading = activeKey != null && !results.has(activeKey)

  const dismiss = useCallback(() => {
    if (triggerKey) setDismissedKey(triggerKey)
  }, [triggerKey])

  const retry = useCallback(() => {
    if (!triggerKey) return
    setResults((prev) => {
      if (!prev.has(triggerKey)) return prev
      const next = new Map(prev)
      next.delete(triggerKey)
      return next
    })
    setDismissedKey((prev) => (prev === triggerKey ? null : prev))
    setRefreshTick((n) => n + 1)
  }, [triggerKey])

  const accept = useCallback(
    async (
      candidate?: Pick<PhantomSuccessorCandidate, "agent_type" | "model">
    ): Promise<PhantomHandoffResponse | null> => {
      const target = candidate ?? offer?.successor
      if (conversationId == null || !target) return null
      setAccepting(true)
      setError(null)
      try {
        const result = await postPhantomHandoff(
          conversationId,
          target.agent_type,
          target.model
        )
        setAccepting(false)
        return result
      } catch (err) {
        setAccepting(false)
        setError(toErrorMessage(err))
        return null
      }
    },
    [conversationId, offer]
  )

  return { offer, accept, dismiss, accepting, error, loading, retry }
}
