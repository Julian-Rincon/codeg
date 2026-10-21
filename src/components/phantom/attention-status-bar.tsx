"use client"

import { Hand } from "lucide-react"
import { useTranslations } from "next-intl"
import { useOptionalWorkbenchRoute } from "@/contexts/workbench-route-context"
import {
  nextBlockedTabId,
  usePhantomAttention,
} from "@/hooks/use-phantom-attention"
import { useTabStore } from "@/stores/tab-store"

/**
 * Status bar counter of tabs waiting on the user. Hidden at zero; each click
 * jumps to the next blocked tab, cycling through them.
 */
export function PhantomAttentionStatusBar() {
  const t = useTranslations("Phantom.attention")
  const { blocked } = usePhantomAttention()
  const switchTab = useTabStore((s) => s.switchTab)
  const route = useOptionalWorkbenchRoute()

  if (blocked.length === 0) return null

  const count = blocked.length
  const handleClick = () => {
    const next = nextBlockedTabId(blocked, useTabStore.getState().activeTabId)
    if (!next) return
    switchTab(next)
    route?.openConversations()
  }

  return (
    <button
      type="button"
      onClick={handleClick}
      aria-label={t("statusBarAria", { count })}
      title={t("statusBarAria", { count })}
      className="inline-flex items-center gap-1 rounded-sm text-amber-600 outline-none hover:text-amber-500 focus-visible:ring-2 focus-visible:ring-ring dark:text-amber-400"
    >
      <Hand className="h-3 w-3" aria-hidden />
      <span>{t("waitingCount", { count })}</span>
    </button>
  )
}
