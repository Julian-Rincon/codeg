"use client"

import { Hand } from "lucide-react"
import { useTranslations } from "next-intl"
import { cn } from "@/lib/utils"
import {
  useConversationBlocked,
  useTabAttention,
} from "@/hooks/use-phantom-attention"

/**
 * The "waiting on you" mark: an amber hand with a tooltip and a screen-reader
 * label, so the state never rides on colour alone.
 */
export function PhantomAttentionIcon({ className }: { className?: string }) {
  const t = useTranslations("Phantom.attention")
  const label = t("waiting")
  return (
    <span
      className={cn("inline-flex shrink-0 items-center", className)}
      title={label}
      data-phantom-attention="blocked"
    >
      <Hand className="h-3 w-3 text-amber-500" aria-hidden />
      <span className="sr-only">{label}</span>
    </span>
  )
}

/** Tab strip badge: shown only while the tab's connection is blocked. */
export function PhantomTabAttentionBadge({ tabId }: { tabId: string }) {
  return useTabAttention(tabId) === "blocked" ? <PhantomAttentionIcon /> : null
}

/** Sidebar row badge: shown while the conversation's open tab is blocked. */
export function PhantomConversationAttentionBadge({
  conversationId,
}: {
  conversationId: number
}) {
  return useConversationBlocked(conversationId) ? (
    <PhantomAttentionIcon className="relative" />
  ) : null
}
