/**
 * Checkpoints / rewind ("Rebobinar aquí") — the frontend half of
 * `src-tauri/src/phantom/checkpoints`.
 *
 * The backend snapshots the working tree before every prompt; this module
 * holds the wire types for the dialog's two calls, and the pure decisions the
 * dialog makes: where in the thread a conversation rewind forks, and what to
 * do when the agent cannot fork there. Nothing here talks to a model.
 */

import { isLiveTurnId } from "@/stores/conversation-runtime-store"

import { getTransport } from "./transport"
import { isTurnInProgressRejection } from "./turn-busy"
import type { MessageTurn } from "./types"

// ── Wire types (`phantom_checkpoint_preview` / `phantom_checkpoint_restore`) ──

export type CheckpointPreviewStatus =
  | "ready"
  | "not_git"
  | "no_checkpoint"
  | "skipped"

export interface CheckpointInfo {
  id: number
  seq: number
  kind: "prompt" | "safety"
  createdAt: string
  changedFiles: number
  skippedReason: string | null
}

export interface CheckpointChangedFile {
  path: string
  /** Relative to the checkpoint: `added` files are deleted by a restore,
   *  `deleted` ones brought back, `modified` ones reverted. */
  status: "added" | "deleted" | "modified"
  additions: number | null
  deletions: number | null
}

export interface CheckpointPreview {
  status: CheckpointPreviewStatus
  checkpoint: CheckpointInfo | null
  files: CheckpointChangedFile[]
  moreFiles: number
  busy: boolean
}

export interface CheckpointRestoreResult {
  safetyCheckpointId: number
  written: number
  deleted: number
}

export function fetchCheckpointPreview(input: {
  conversationId: number
  messageId: string | null
  turnTimestamp: string | null
  turnText: string | null
}): Promise<CheckpointPreview> {
  return getTransport().call<CheckpointPreview>(
    "phantom_checkpoint_preview",
    input
  )
}

export function restoreCheckpoint(
  conversationId: number,
  checkpointId: number
): Promise<CheckpointRestoreResult> {
  return getTransport().call<CheckpointRestoreResult>(
    "phantom_checkpoint_restore",
    { conversationId, checkpointId }
  )
}

/** A restore refused because a turn is running (HTTP 409 / `turn_in_progress`). */
export function isCheckpointBusyError(err: unknown): boolean {
  return isTurnInProgressRejection(err)
}

/** Totals for the dialog's summary line. Binary files count as changed but
 *  add no lines. */
export function summarizeChangedFiles(
  files: CheckpointChangedFile[],
  moreFiles = 0
): { files: number; additions: number; deletions: number } {
  return files.reduce(
    (acc, f) => ({
      files: acc.files,
      additions: acc.additions + (f.additions ?? 0),
      deletions: acc.deletions + (f.deletions ?? 0),
    }),
    { files: files.length + moreFiles, additions: 0, deletions: 0 }
  )
}

// ── Where a conversation rewind forks ────────────────────────────────────

/**
 * Rewinding the conversation to a user message keeps everything BEFORE it —
 * i.e. forks at the reply that precedes it, using codeg's "fork from here"
 * (which forks up to and including an assistant turn).
 *
 * - `fork`: fork at `turnId`, the backend's name for that reply.
 * - `first`: nothing precedes this message; the rewound conversation is empty.
 * - `unnamed`: the preceding reply streamed in this session and the backend
 *   has not been told its name yet, so a fork would land at the wrong place.
 * - `unloaded`: the preceding reply is in older history not loaded yet.
 */
export type RewindForkTarget =
  | { kind: "fork"; turnId: string }
  | { kind: "first" }
  | { kind: "unnamed" }
  | { kind: "unloaded" }

/** The slice of a rendered thread item the fork-target search reads. */
export interface RewindThreadItem {
  key: string
  kind: string
  group?: { role: string }
  sourceTurns?: MessageTurn[]
}

export function findRewindForkTarget(
  items: readonly RewindThreadItem[],
  userItemKey: string,
  hasOlderTurns: boolean
): RewindForkTarget {
  const index = items.findIndex((item) => item.key === userItemKey)
  if (index < 0) return { kind: "unloaded" }
  for (let i = index - 1; i >= 0; i -= 1) {
    const item = items[i]
    if (item.kind !== "turn" || item.group?.role !== "assistant") continue
    const turns = item.sourceTurns ?? []
    const last = turns[turns.length - 1]
    if (!last) continue
    if (last.source_turn_id) {
      return { kind: "fork", turnId: last.source_turn_id }
    }
    if (isLiveTurnId(last.id)) return { kind: "unnamed" }
    return { kind: "fork", turnId: last.id }
  }
  return hasOlderTurns ? { kind: "unloaded" } : { kind: "first" }
}

// ── What a conversation rewind will actually do ──────────────────────────

/**
 * - `fork`: fork this conversation at the reply before the message; the
 *   message's text goes back into this conversation's composer.
 * - `fresh`: open a NEW conversation (same agent, same folder) with the text
 *   in its composer — the fallback when forking there is impossible.
 *   `reason` says why, for the dialog to explain.
 * - `blocked`: nothing sensible can be done yet (older history not loaded).
 */
export type RewindConversationPlan =
  | { mode: "fork"; turnId: string }
  | { mode: "fresh"; reason: "first" | "unsupported" | "unnamed" }
  | { mode: "blocked"; reason: "unloaded" }

export function planConversationRewind(
  target: RewindForkTarget,
  canFork: boolean
): RewindConversationPlan {
  switch (target.kind) {
    case "first":
      return { mode: "fresh", reason: "first" }
    case "unloaded":
      return { mode: "blocked", reason: "unloaded" }
    case "unnamed":
      return { mode: "fresh", reason: canFork ? "unnamed" : "unsupported" }
    case "fork":
      return canFork
        ? { mode: "fork", turnId: target.turnId }
        : { mode: "fresh", reason: "unsupported" }
  }
}

/**
 * What the conversation surface hosting the transcript provides. Only the
 * owning conversation panel passes one; every other embed omits it and the
 * action is not offered.
 */
export interface PhantomRewindHost {
  /** The persisted conversation id. */
  conversationId: number
  /** The live agent can fork a session at a turn (`session/fork`). */
  canFork: boolean
  /** Carry out a conversation plan, putting `text` in the right composer.
   *  Resolves `false` when it failed (already reported to the user). */
  rewindConversation: (
    plan: Exclude<RewindConversationPlan, { mode: "blocked" }>,
    text: string
  ) => Promise<boolean>
}

/** The host plus the thread lookup the message list binds in. */
export interface PhantomRewindContext extends PhantomRewindHost {
  resolveForkTarget: (userItemKey: string) => RewindForkTarget
}
