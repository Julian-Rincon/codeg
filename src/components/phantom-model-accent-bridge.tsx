"use client"

import { useEffect, useMemo } from "react"
import { useConnection } from "@/hooks/use-connection"
import { useModelLabels } from "@/hooks/use-model-labels"
import { useTabStore } from "@/contexts/tab-context"
import {
  applyPhantomAccent,
  currentModelId,
  persistPhantomAccent,
  resolvePhantomAccent,
} from "@/lib/phantom-ui"

/**
 * Keeps the visible Phantom accent tied to the model on the focused ACP
 * connection. It is intentionally mounted once at workspace level instead of
 * inside every conversation: split/tile views still have one focused tab, and
 * a single root accent avoids competing global theme writers.
 *
 * With no connection signal yet (no model and no agent) it leaves the accent
 * restored by the pre-hydration script alone instead of resetting to blue.
 */
export function PhantomModelAccentBridge() {
  const activeTabId = useTabStore((state) => state.activeTabId)
  const activeAgentType = useTabStore(
    (state) =>
      state.tabs.find((tab) => tab.id === activeTabId)?.agentType ?? null
  )
  const connection = useConnection(activeTabId ?? "")
  const agentType = connection.agentType ?? activeAgentType
  const modelId = useMemo(
    () => currentModelId(connection.configOptions),
    [connection.configOptions]
  )
  const resolveModelLabel = useModelLabels(agentType)
  const modelLabel = modelId ? resolveModelLabel(modelId) : null
  const accent = useMemo(
    () => resolvePhantomAccent({ modelId, modelLabel, agentType }),
    [agentType, modelId, modelLabel]
  )

  const hasSignal = Boolean(modelId || agentType)

  useEffect(() => {
    if (!hasSignal) return
    applyPhantomAccent(accent, document.documentElement)
    persistPhantomAccent(accent.id)
  }, [accent, hasSignal])

  return null
}
