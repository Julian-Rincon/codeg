"use client"

/**
 * Token-usage segment of the status bar: the Claude plan quota at a glance
 * ("Claude 77% · wk 62%", tinted by severity), and — in the popover — the two
 * quota windows with their local reset times plus today's tokens per
 * agent/model.
 *
 * Read-only and best-effort: the fetch returns `null` on any failure (old
 * server, no OAuth credentials), and the whole segment hides rather than
 * showing a broken indicator. Refreshes every two minutes.
 */

import { useCallback, useEffect, useRef, useState } from "react"
import { useLocale, useTranslations } from "next-intl"

import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { getAgentLabel } from "@/lib/custom-agents"
import { formatLimitResetTime } from "@/lib/model-scorecard"
import {
  fetchUsageMeter,
  formatTokens,
  usageLevel,
  type UsageLevel,
  type UsageMeterResponse,
  type UsageMeterWindow,
} from "@/lib/usage-meter"
import { cn } from "@/lib/utils"

/** Background refresh cadence — the meter is a plan quota, not a live stream. */
const POLL_MS = 2 * 60 * 1000

const LEVEL_TEXT: Record<UsageLevel, string> = {
  ok: "text-emerald-500",
  warn: "text-amber-500",
  high: "text-red-500",
}

const LEVEL_BAR: Record<UsageLevel, string> = {
  ok: "bg-emerald-500",
  warn: "bg-amber-500",
  high: "bg-red-500",
}

export function StatusBarUsage() {
  const t = useTranslations("Folder.statusBar.usage")
  const [meter, setMeter] = useState<UsageMeterResponse | null>(null)
  // Guards against a slow poll landing after unmount or overwriting a fresher
  // result (same shape as `status-bar-mcp.tsx`).
  const seqRef = useRef(0)
  const aliveRef = useRef(true)

  const refresh = useCallback(async () => {
    const seq = ++seqRef.current
    const next = await fetchUsageMeter()
    if (!aliveRef.current || seq !== seqRef.current) return
    setMeter(next)
  }, [])

  useEffect(() => {
    aliveRef.current = true
    // Initial read; the state write lands after the await below (and the
    // interval only ticks later), never synchronously in the effect.
    // eslint-disable-next-line react-hooks/set-state-in-effect -- initial fetch on mount
    void refresh()
    const id = setInterval(() => void refresh(), POLL_MS)
    return () => {
      aliveRef.current = false
      clearInterval(id)
    }
  }, [refresh])

  const claude = meter?.claude ?? null
  const today = meter?.today ?? []
  const fiveHour = claude?.five_hour ?? null
  const week = claude?.seven_day ?? null
  const weekTokens = meter?.week_tokens ?? 0
  const weekPercent = week ? Math.round(week.percent) : null

  if (!meter || (!claude && today.length === 0)) return null

  const level: UsageLevel = fiveHour ? usageLevel(fiveHour.percent) : "ok"
  const compact =
    claude && fiveHour
      ? t("compact", {
          agent: t("agent"),
          fiveHour: Math.round(fiveHour.percent),
          week: weekPercent ?? 0,
        })
      : t("compactTokens", {
          tokens: formatTokens(
            today.reduce(
              (sum, entry) => sum + entry.input_tokens + entry.output_tokens,
              0
            )
          ),
        })

  return (
    <Popover>
      <PopoverTrigger asChild>
        <button
          aria-label={t("ariaLabel")}
          title={t("title")}
          className={cn(
            "flex items-center gap-1 tabular-nums transition-colors hover:opacity-80",
            claude ? LEVEL_TEXT[level] : "text-muted-foreground"
          )}
        >
          {compact}
        </button>
      </PopoverTrigger>
      <PopoverContent side="top" align="end" className="w-72 gap-3 p-3">
        <div className="flex items-center justify-between gap-2 text-xs font-medium">
          <span>{t("title")}</span>
          {claude?.plan && (
            <span className="text-3xs font-normal text-muted-foreground">
              {t("plan", { plan: claude.plan })}
            </span>
          )}
        </div>

        {claude && (fiveHour || week) && (
          <div className="space-y-3">
            {fiveHour && <QuotaBar label={t("fiveHour")} window={fiveHour} />}
            {week && <QuotaBar label={t("week")} window={week} />}
          </div>
        )}

        {meter.claude_error && (
          <p className="text-2xs break-words text-destructive">
            {t("claudeError", { message: meter.claude_error })}
          </p>
        )}

        {today.length > 0 ? (
          <div className="space-y-1">
            <div className="text-2xs font-medium text-muted-foreground">
              {t("today")}
            </div>
            {today.map((entry, index) => (
              <div
                key={`${entry.agent_type}:${entry.model}:${index}`}
                className="flex items-center justify-between gap-2 text-2xs"
              >
                <span className="truncate">
                  {getAgentLabel(entry.agent_type)}
                  {entry.model ? ` · ${entry.model}` : ""}
                </span>
                <span className="shrink-0 tabular-nums">
                  {formatTokens(entry.input_tokens + entry.output_tokens)}
                </span>
              </div>
            ))}
          </div>
        ) : (
          <p className="text-2xs text-muted-foreground">{t("empty")}</p>
        )}

        {weekTokens > 0 && (
          <div className="text-2xs text-muted-foreground">
            {t("weekTokens", { tokens: formatTokens(weekTokens) })}
          </div>
        )}
      </PopoverContent>
    </Popover>
  )
}

function QuotaBar({
  label,
  window,
}: {
  label: string
  window: UsageMeterWindow
}) {
  const t = useTranslations("Folder.statusBar.usage")
  const locale = useLocale()
  const percent = Math.max(0, Math.min(100, window.percent))
  const reset = formatLimitResetTime(window.resets_at, locale)
  return (
    <div className="space-y-1">
      <div className="flex items-center justify-between text-2xs text-muted-foreground">
        <span>{label}</span>
        <span className="tabular-nums">{Math.round(window.percent)}%</span>
      </div>
      <div className="h-1.5 rounded-full bg-muted">
        <div
          className={cn(
            "h-full rounded-full transition-all",
            LEVEL_BAR[usageLevel(window.percent)]
          )}
          style={{ width: `${percent}%` }}
        />
      </div>
      {reset && (
        <div className="text-3xs text-muted-foreground">
          {t("resetsAt", { time: reset })}
        </div>
      )}
    </div>
  )
}
