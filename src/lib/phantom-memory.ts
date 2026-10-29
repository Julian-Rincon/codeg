"use client"

/**
 * Phantom's memory editor — the client half of Claude Code's `/memory` and its
 * `#` quick-add, for Claude Code sessions.
 *
 * The backend (`commands::phantom_memory`) owns which files are reachable: the
 * client names a workspace folder and a scope, never a path. This module holds
 * the wire types, the transport calls, the quick-add parser and the device-
 * local "quick-add on/off" preference.
 */

import { useSyncExternalStore } from "react"
import { getTransport } from "@/lib/transport"
import type { AgentType, PromptDraft } from "@/lib/types"

export type MemoryScope = "user" | "project" | "project_dot_claude" | "local"

/** The scopes the `#` quick-add chooser offers, in Claude Code's order. */
export const QUICK_ADD_SCOPES = ["project", "user"] as const
export type QuickAddScope = (typeof QUICK_ADD_SCOPES)[number]

export interface MemoryFileInfo {
  scope: MemoryScope
  path: string
  exists: boolean
  mtime_ms?: number
  size_bytes?: number
  error?: string
}

export interface MemoryFileContent {
  scope: MemoryScope
  path: string
  exists: boolean
  content: string
  mtime_ms?: number
}

export type MemoryWriteResult =
  | { status: "saved"; mtime_ms: number }
  | { status: "conflict"; current_mtime_ms?: number }

export interface MemoryAppendResult {
  scope: MemoryScope
  path: string
  mtime_ms: number
  created: boolean
}

export function listMemoryFiles(folderPath: string) {
  return getTransport().call<MemoryFileInfo[]>("phantom_memory_list", {
    folderPath,
  })
}

export function readMemoryFile(folderPath: string, scope: MemoryScope) {
  return getTransport().call<MemoryFileContent>("phantom_memory_read", {
    folderPath,
    scope,
  })
}

export function writeMemoryFile(
  folderPath: string,
  scope: MemoryScope,
  content: string,
  expectedMtimeMs: number | null
) {
  return getTransport().call<MemoryWriteResult>("phantom_memory_write", {
    folderPath,
    scope,
    content,
    expectedMtimeMs,
  })
}

export function appendMemoryNote(
  folderPath: string,
  scope: MemoryScope,
  text: string
) {
  return getTransport().call<MemoryAppendResult>("phantom_memory_append", {
    folderPath,
    scope,
    text,
  })
}

/** Memory files are a Claude Code concept; other agents read other files. */
export function supportsClaudeMemory(
  agentType: AgentType | null | undefined
): boolean {
  return agentType === "claude_code"
}

/**
 * The note a draft quick-adds, or `null` when the draft is an ordinary message.
 *
 * Claude Code's convention: the WHOLE input is one line starting with `#`. So
 * the draft must be a single plain-text block (no images, files or mentions),
 * contain no line break, and have something after the `#`. Exactly one `#` is
 * stripped, as Claude Code does.
 */
export function parseMemoryQuickAdd(draft: PromptDraft | null): string | null {
  if (!draft || draft.blocks.length !== 1) return null
  const block = draft.blocks[0]
  if (block.type !== "text") return null
  const text = block.text.trim()
  if (!text.startsWith("#") || /[\r\n]/.test(text)) return null
  const note = text.slice(1).trim()
  return note.length > 0 ? note : null
}

/** What the appended line looks like, for the chooser's preview. */
export function formatMemoryBullet(note: string): string {
  return `- ${note}`
}

// ─── "#" quick-add preference (device-local, like the other composer prefs) ───

const QUICK_ADD_KEY = "phantom:memory-quick-add:v1"
const QUICK_ADD_EVENT = "phantom:memory-quick-add-changed"

export function getMemoryQuickAddEnabled(): boolean {
  if (typeof window === "undefined") return true
  try {
    return window.localStorage.getItem(QUICK_ADD_KEY) !== "off"
  } catch {
    return true
  }
}

export function setMemoryQuickAddEnabled(enabled: boolean): void {
  try {
    if (enabled) window.localStorage.removeItem(QUICK_ADD_KEY)
    else window.localStorage.setItem(QUICK_ADD_KEY, "off")
  } catch {
    // Storage unavailable (private mode): the toggle just won't persist.
  }
  window.dispatchEvent(new Event(QUICK_ADD_EVENT))
}

function subscribeQuickAdd(cb: () => void): () => void {
  const onStorage = (e: StorageEvent) => {
    if (e.key === QUICK_ADD_KEY) cb()
  }
  window.addEventListener(QUICK_ADD_EVENT, cb)
  window.addEventListener("storage", onStorage)
  return () => {
    window.removeEventListener(QUICK_ADD_EVENT, cb)
    window.removeEventListener("storage", onStorage)
  }
}

export function useMemoryQuickAddEnabled(): boolean {
  return useSyncExternalStore(
    subscribeQuickAdd,
    getMemoryQuickAddEnabled,
    () => true
  )
}
