"use client"

import { useCallback, useMemo, useSyncExternalStore } from "react"
import {
  useOptionalConnectionStore,
  type ConnectionState,
} from "@/contexts/acp-connections-context"
import { blockingPrompts } from "@/lib/blocking-prompts"
import { useTabStore, type TabItem } from "@/stores/tab-store"

/**
 * Phantom "needs me" signal: which open conversation tabs are parked on the
 * user (a permission, an `ask_user_question`, a plan approval — exactly what
 * `blockingPrompts` reports) and which are mid-turn.
 *
 * Only tabs open in THIS client are covered: the connection store is keyed by
 * tab id, so a conversation with no tab here has no live state to read.
 */
export type TabAttention = "blocked" | "running"

export function attentionOf(
  conn: ConnectionState | null | undefined
): TabAttention | null {
  if (!conn) return null
  if (blockingPrompts(conn)) return "blocked"
  if (conn.status === "prompting") return "running"
  return null
}

export interface PhantomAttention {
  /** Blocked tabs, in tab-strip order. */
  blocked: TabItem[]
  /** Running (and not blocked) tabs, in tab-strip order. */
  running: TabItem[]
}

const EMPTY: PhantomAttention = { blocked: [], running: [] }
const SEP = "\n"

/**
 * Attention state for every open tab. Subscribes to each tab's connection key
 * (re-subscribing as tabs open and close) and re-renders only when some tab's
 * attention actually changes — the snapshot is a plain string signature, so a
 * streaming token on a running tab doesn't re-render consumers.
 */
export function usePhantomAttention(): PhantomAttention {
  const store = useOptionalConnectionStore()
  const tabs = useTabStore((s) => s.tabs)
  const idsKey = useMemo(() => tabs.map((t) => t.id).join(SEP), [tabs])

  const subscribe = useCallback(
    (cb: () => void) => {
      if (!store || !idsKey) return () => {}
      const unsubs = idsKey.split(SEP).map((id) => store.subscribeKey(id, cb))
      return () => unsubs.forEach((unsub) => unsub())
    },
    [store, idsKey]
  )
  const getSnapshot = useCallback(() => {
    if (!store || !idsKey) return ""
    return idsKey
      .split(SEP)
      .map((id) => {
        const state = attentionOf(store.getConnection(id))
        return state ? `${state}:${id}` : ""
      })
      .join(SEP)
  }, [store, idsKey])
  const signature = useSyncExternalStore(subscribe, getSnapshot, getSnapshot)

  return useMemo(() => {
    if (!signature.trim()) return EMPTY
    const byId = new Map<string, TabAttention>()
    for (const entry of signature.split(SEP)) {
      if (!entry) continue
      const at = entry.indexOf(":")
      byId.set(entry.slice(at + 1), entry.slice(0, at) as TabAttention)
    }
    return {
      blocked: tabs.filter((t) => byId.get(t.id) === "blocked"),
      running: tabs.filter((t) => byId.get(t.id) === "running"),
    }
  }, [signature, tabs])
}

/** Attention state of one tab's connection, or `null` when it needs nothing. */
export function useTabAttention(
  tabId: string | null | undefined
): TabAttention | null {
  const store = useOptionalConnectionStore()
  const subscribe = useCallback(
    (cb: () => void) => {
      if (!store || !tabId) return () => {}
      return store.subscribeKey(tabId, cb)
    },
    [store, tabId]
  )
  const getSnapshot = useCallback(
    () => (store && tabId ? attentionOf(store.getConnection(tabId)) : null),
    [store, tabId]
  )
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot)
}

/**
 * Whether the open tab showing `conversationId` is blocked. Only conversations
 * with a tab in this client can answer yes.
 */
export function useConversationBlocked(conversationId: number): boolean {
  const tabId = useTabStore(
    (s) => s.tabs.find((t) => t.conversationId === conversationId)?.id ?? null
  )
  return useTabAttention(tabId) === "blocked"
}

/**
 * The blocked tab to jump to next: the one after the active tab in `blocked`
 * order (wrapping), or the first when the active tab isn't blocked.
 */
export function nextBlockedTabId(
  blocked: readonly Pick<TabItem, "id">[],
  activeTabId: string | null
): string | null {
  if (blocked.length === 0) return null
  const idx = blocked.findIndex((t) => t.id === activeTabId)
  return blocked[(idx + 1) % blocked.length].id
}
