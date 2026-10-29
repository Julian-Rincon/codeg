"use client"

import { useCallback, useEffect, useMemo } from "react"

import type { PhantomRewindHost } from "@/lib/phantom-checkpoints"
import type { AgentType } from "@/lib/types"

/**
 * The conversation half of "Rebobinar aquí", for the conversation panel that
 * owns the transcript. Kept out of that (upstream) panel: it only hands over
 * the few capabilities this needs.
 */

// ── Hand-off of the message text to a freshly opened conversation tab ──────

interface ParkedRewindDraft {
  text: string
  agentType: AgentType
  folderId: number
}

const parked = new Map<string, ParkedRewindDraft>()
const PARKED_EVENT = "phantom:rewind-draft-parked"

/** Park `entry` for the draft tab `tabId`; that tab's
 *  {@link usePhantomRewindDraftInbox} puts it in its composer. */
export function parkRewindDraft(tabId: string, entry: ParkedRewindDraft) {
  parked.set(tabId, entry)
  if (typeof window === "undefined") return
  window.dispatchEvent(new CustomEvent(PARKED_EVENT, { detail: { tabId } }))
}

/** Take the text parked for `tabId` once the tab has become the agent and
 *  folder it was parked for — a reused draft tab retargets asynchronously,
 *  and text injected before that lands would go to the old composer. */
export function consumeRewindDraft(
  tabId: string,
  current: { agentType: AgentType | null; folderId: number | null }
): string | null {
  const entry = parked.get(tabId)
  if (!entry) return null
  if (
    entry.agentType !== current.agentType ||
    entry.folderId !== current.folderId
  ) {
    return null
  }
  parked.delete(tabId)
  return entry.text
}

/** Receiving end, mounted by every conversation panel. */
export function usePhantomRewindDraftInbox(
  tabId: string,
  agentType: AgentType | null,
  folderId: number | null,
  injectText: (text: string) => void
) {
  useEffect(() => {
    const drain = () => {
      const text = consumeRewindDraft(tabId, { agentType, folderId })
      if (text) injectText(text)
    }
    drain()
    const onParked = (event: Event) => {
      const detail = (event as CustomEvent<{ tabId: string }>).detail
      if (detail?.tabId === tabId) drain()
    }
    window.addEventListener(PARKED_EVENT, onParked)
    return () => window.removeEventListener(PARKED_EVENT, onParked)
  }, [agentType, folderId, injectText, tabId])
}

// ── The host object handed to the message list ──────────────────────────

export interface UsePhantomRewindHostInput {
  /** Persisted conversation id; `null` hides the action. */
  conversationId: number | null
  canFork: boolean
  /** codeg's "fork from here" — resolves `true` once the fork landed. */
  forkFromTurn: (turnId: string) => Promise<boolean>
  /** Put text into THIS conversation's composer. */
  injectText: (text: string) => void
  /** Open a new conversation draft with the same agent and folder; returns
   *  its tab and the identity it was promised, or `null` when impossible. */
  openFreshConversation: () => {
    tabId: string
    agentType: AgentType
    folderId: number
  } | null
}

export function usePhantomRewindHost({
  conversationId,
  canFork,
  forkFromTurn,
  injectText,
  openFreshConversation,
}: UsePhantomRewindHostInput): PhantomRewindHost | undefined {
  const rewindConversation = useCallback<
    PhantomRewindHost["rewindConversation"]
  >(
    async (plan, text) => {
      if (plan.mode === "fork") {
        const forked = await forkFromTurn(plan.turnId)
        if (forked && text) injectText(text)
        return forked
      }
      const target = openFreshConversation()
      if (!target) return false
      if (text) {
        parkRewindDraft(target.tabId, {
          text,
          agentType: target.agentType,
          folderId: target.folderId,
        })
      }
      return true
    },
    [forkFromTurn, injectText, openFreshConversation]
  )

  return useMemo(
    () =>
      conversationId == null
        ? undefined
        : { conversationId, canFork, rewindConversation },
    [canFork, conversationId, rewindConversation]
  )
}
