"use client"

/**
 * Phantom prompt history: every prompt the user sent from any composer,
 * persisted per device so Up-arrow recall and the Ctrl+R search reach prompts
 * from other conversations (Claude Code's own history works the same way).
 *
 * Upstream's Up/Down recall (`composer-history.ts`) is per-session and derived
 * from the session's turns; `recallHistory` extends that list with this store
 * instead of replacing it, so the session's own prompts still come first.
 */

const STORAGE_KEY = "phantom:prompt-history:v1"
export const PROMPT_HISTORY_MAX = 200

export interface PromptHistoryEntry {
  text: string
  /** Working directory the prompt was sent from; `null` when unknown. */
  folder: string | null
  /** Epoch ms of the last send. */
  at: number
}

/**
 * `entries` (most recent first) with `text` moved/added to the front. A prompt
 * sent again is one entry, re-stamped with its newest folder and time.
 */
export function addPromptHistoryEntry(
  entries: readonly PromptHistoryEntry[],
  entry: PromptHistoryEntry,
  max = PROMPT_HISTORY_MAX
): PromptHistoryEntry[] {
  const text = entry.text.trim()
  if (!text) return [...entries]
  return [{ ...entry, text }, ...entries.filter((e) => e.text !== text)].slice(
    0,
    max
  )
}

/**
 * Most recent first, the current folder's prompts ahead of everyone else's
 * (each group keeps its recency order).
 */
export function rankPromptHistory(
  entries: readonly PromptHistoryEntry[],
  folder: string | null
): PromptHistoryEntry[] {
  const sorted = [...entries].sort((a, b) => b.at - a.at)
  if (!folder) return sorted
  return [
    ...sorted.filter((e) => e.folder === folder),
    ...sorted.filter((e) => e.folder !== folder),
  ]
}

/**
 * The Up/Down recall list, OLDEST first (the shape `stepComposerHistory`
 * walks from the end): the session's own prompts are the newest, then the
 * stored history ranked by `rankPromptHistory`, duplicates dropped.
 */
export function recallHistory(
  sessionOldestFirst: readonly string[],
  entries: readonly PromptHistoryEntry[],
  folder: string | null,
  max = PROMPT_HISTORY_MAX
): string[] {
  const seen = new Set<string>()
  const newestFirst: string[] = []
  const push = (text: string) => {
    const t = text.trim()
    if (!t || seen.has(t)) return
    seen.add(t)
    newestFirst.push(t)
  }
  for (let i = sessionOldestFirst.length - 1; i >= 0; i--) {
    push(sessionOldestFirst[i])
  }
  for (const e of rankPromptHistory(entries, folder)) push(e.text)
  return newestFirst.slice(0, max).reverse()
}

/**
 * Score `text` against `query`: a contiguous (case-insensitive) match beats a
 * word-by-word match, which beats a fuzzy subsequence; `null` when it doesn't
 * match at all. Lower is better.
 */
function matchScore(text: string, query: string): number | null {
  const hay = text.toLowerCase()
  const q = query.toLowerCase()
  const at = hay.indexOf(q)
  if (at >= 0) return at === 0 ? 0 : 1
  const words = q.split(/\s+/).filter(Boolean)
  if (words.length > 1 && words.every((w) => hay.includes(w))) return 2
  let from = 0
  for (const ch of q.replace(/\s+/g, "")) {
    const idx = hay.indexOf(ch, from)
    if (idx < 0) return null
    from = idx + 1
  }
  return 3
}

/**
 * Prompts matching `query`, best match first and, within a match class, in the
 * order `ranked` already has them (recency, current folder first). An empty
 * query returns `ranked` as is.
 */
export function searchPromptHistory(
  ranked: readonly string[],
  query: string,
  limit = 50
): string[] {
  const q = query.trim()
  if (!q) return ranked.slice(0, limit)
  const scored: { text: string; score: number; order: number }[] = []
  ranked.forEach((text, order) => {
    const score = matchScore(text, q)
    if (score !== null) scored.push({ text, score, order })
  })
  scored.sort((a, b) => a.score - b.score || a.order - b.order)
  return scored.slice(0, limit).map((s) => s.text)
}

function isEntry(value: unknown): value is PromptHistoryEntry {
  if (!value || typeof value !== "object") return false
  const v = value as Record<string, unknown>
  return (
    typeof v.text === "string" &&
    typeof v.at === "number" &&
    (v.folder === null || typeof v.folder === "string")
  )
}

export function loadPromptHistory(): PromptHistoryEntry[] {
  if (typeof window === "undefined") return []
  try {
    const raw = localStorage.getItem(STORAGE_KEY)
    if (!raw) return []
    const parsed: unknown = JSON.parse(raw)
    return Array.isArray(parsed) ? parsed.filter(isEntry) : []
  } catch {
    return []
  }
}

/** Remember a sent prompt. Never throws: history is a convenience. */
export function recordPrompt(
  text: string,
  folder: string | null,
  now = Date.now()
): void {
  if (typeof window === "undefined" || !text.trim()) return
  try {
    const next = addPromptHistoryEntry(loadPromptHistory(), {
      text,
      folder,
      at: now,
    })
    localStorage.setItem(STORAGE_KEY, JSON.stringify(next))
  } catch {
    // Quota or privacy mode: skip silently.
  }
}

/** The composer's Up/Down list: session prompts, then stored history. */
export function loadRecallHistory(
  sessionOldestFirst: readonly string[],
  folder: string | null
): string[] {
  return recallHistory(sessionOldestFirst, loadPromptHistory(), folder)
}

// ── Composer key handling ──

interface KeyLike {
  key: string
  ctrlKey?: boolean
  metaKey?: boolean
  altKey?: boolean
  shiftKey?: boolean
  isComposing?: boolean
  keyCode?: number
}

/**
 * Ctrl+R (Ctrl on every platform, like Claude Code — ⌘R stays the browser's
 * reload) outside an IME composition.
 */
export function isPromptSearchShortcut(event: KeyLike): boolean {
  if (event.isComposing || event.keyCode === 229) return false
  return (
    event.ctrlKey === true &&
    !event.metaKey &&
    !event.altKey &&
    !event.shiftKey &&
    event.key.toLowerCase() === "r"
  )
}

/** Whether the composer may open the search: no other menu owns the keys. */
export function canOpenPromptSearch(state: {
  slashMenuOpen: boolean
  mentionMenuOpen: boolean
  editingQueueItem: boolean
}): boolean {
  return (
    !state.slashMenuOpen && !state.mentionMenuOpen && !state.editingQueueItem
  )
}

export type PromptSearchKeyAction =
  | { type: "move"; index: number }
  | { type: "select" }
  | { type: "close" }
  | null

/** Key handling inside the Ctrl+R search box. */
export function promptSearchKeyAction(
  event: KeyLike,
  selected: number,
  count: number
): PromptSearchKeyAction {
  if (event.isComposing || event.keyCode === 229) return null
  switch (event.key) {
    case "Escape":
      return { type: "close" }
    case "Enter":
      return count > 0 ? { type: "select" } : null
    case "ArrowDown":
      return count > 0 ? { type: "move", index: (selected + 1) % count } : null
    case "ArrowUp":
      return count > 0
        ? { type: "move", index: (selected - 1 + count) % count }
        : null
    default:
      // Ctrl+R again steps to the next (older) match, as in a shell.
      if (isPromptSearchShortcut(event) && count > 0) {
        return { type: "move", index: (selected + 1) % count }
      }
      return null
  }
}
