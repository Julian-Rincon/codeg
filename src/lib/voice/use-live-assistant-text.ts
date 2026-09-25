"use client"

// Reads the active turn's accumulating assistant text straight from the
// conversation runtime store, as PLAIN text for the sentence chunker/TTS
// queue. Deliberately a narrow, conversationId-scoped selector rather than
// something the composer's parent panel reads and passes down as a prop:
// `liveMessage` is rewritten on every streaming batch (~60/s — see the
// perf note in `conversation-detail-panel.tsx` around its own liveMessage
// subscription), so this hook is meant to be called from a leaf that's cheap
// to re-render (the composer), not from an ancestor that would drag the
// whole subtree along with it.
//
// `enabled` additionally gates the subscription's OUTPUT (not its
// existence — hooks can't be conditional): while `false` (voice mode
// closed), the selector always returns "" so the calling component never
// re-renders due to streaming tokens it doesn't care about yet.

import { useConversationRuntimeStore } from "@/stores/conversation-runtime-store"
import type { LiveContentBlock } from "@/contexts/acp-connections-context"

function extractPlainText(blocks: LiveContentBlock[] | undefined): string {
  if (!blocks || blocks.length === 0) return ""
  let text = ""
  for (const block of blocks) {
    if (block.type === "text") text += block.text
  }
  return text
}

export function useLiveAssistantText(
  conversationId: number | null,
  enabled: boolean
): string {
  return useConversationRuntimeStore((s) => {
    if (!enabled || conversationId == null) return ""
    const session = s.byConversationId.get(conversationId)
    return extractPlainText(session?.liveMessage?.content)
  })
}
