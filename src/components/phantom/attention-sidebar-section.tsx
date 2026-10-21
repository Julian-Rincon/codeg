"use client"

import { Loader2 } from "lucide-react"
import { useTranslations } from "next-intl"
import { AgentIcon } from "@/components/agent-icon"
import { PhantomAttentionIcon } from "@/components/phantom/attention-badges"
import { useSidebarContext } from "@/contexts/sidebar-context"
import { useWorkbenchRoute } from "@/contexts/workbench-route-context"
import { useIsMobile } from "@/hooks/use-mobile"
import { usePhantomAttention } from "@/hooks/use-phantom-attention"
import { cn } from "@/lib/utils"
import { useTabStore, type TabItem } from "@/stores/tab-store"

const MAX_ROWS = 6

/**
 * "Activas": open tabs that need the user (first) or are mid-turn, pinned to
 * the top of the sidebar's conversation area. Renders nothing when no tab is
 * blocked or running.
 */
export function PhantomActiveSection() {
  const t = useTranslations("Phantom.attention")
  const { blocked, running } = usePhantomAttention()
  const activeTabId = useTabStore((s) => s.activeTabId)
  const switchTab = useTabStore((s) => s.switchTab)
  const { openConversations } = useWorkbenchRoute()
  const { toggle } = useSidebarContext()
  const isMobile = useIsMobile()

  const rows: { tab: TabItem; blocked: boolean }[] = [
    ...blocked.map((tab) => ({ tab, blocked: true })),
    ...running.map((tab) => ({ tab, blocked: false })),
  ]
  if (rows.length === 0) return null
  const hidden = rows.length - MAX_ROWS

  const activate = (tabId: string) => {
    switchTab(tabId)
    openConversations()
    if (isMobile) toggle()
  }

  return (
    <section
      aria-label={t("activeSection")}
      className="flex shrink-0 flex-col px-1.5 pb-1.5"
    >
      <div className="px-[0.5rem] py-[0.375rem] text-[0.75rem] text-muted-foreground/70">
        {t("activeSection")}
      </div>
      {rows.slice(0, MAX_ROWS).map(({ tab, blocked: isBlocked }) => (
        <button
          key={tab.id}
          type="button"
          onClick={() => activate(tab.id)}
          title={tab.title}
          aria-current={tab.id === activeTabId ? "true" : undefined}
          className={cn(
            "flex h-8 w-full items-center gap-[0.4375rem] rounded-full pl-[0.4375rem] pr-1.5",
            "text-[0.875rem] text-sidebar-foreground outline-none",
            "transition-colors duration-150 hover:bg-sidebar-accent",
            "focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-inset",
            tab.id === activeTabId && "bg-sidebar-primary/8"
          )}
        >
          <AgentIcon
            agentType={tab.agentType}
            className="h-[0.875rem] w-[0.875rem] shrink-0"
          />
          <span className="min-w-0 flex-1 truncate text-left">{tab.title}</span>
          {isBlocked ? (
            <PhantomAttentionIcon />
          ) : (
            <span
              className="inline-flex shrink-0 items-center"
              title={t("running")}
            >
              <Loader2
                className="h-3 w-3 animate-spin text-muted-foreground"
                aria-hidden
              />
              <span className="sr-only">{t("running")}</span>
            </span>
          )}
        </button>
      ))}
      {hidden > 0 ? (
        <div className="px-[0.5rem] py-[0.25rem] text-[0.75rem] text-muted-foreground/70">
          {t("more", { count: hidden })}
        </div>
      ) : null}
    </section>
  )
}
