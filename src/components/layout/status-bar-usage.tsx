"use client"

/**
 * Token-usage segment of the status bar: the Claude plan quota at a glance
 * ("Claude 77% · wk 62%", tinted by severity), and — in the popover — the two
 * quota windows with their local reset times, today's tokens per agent/model,
 * and the delegation router's profile switch.
 *
 * Read-only apart from the profile switch, and best-effort: the fetch returns
 * `null` on any failure (old server, no OAuth credentials), and the whole
 * segment hides rather than showing a broken indicator. Refreshes every two
 * minutes, and sooner (throttled) whenever a session reports `usage_update`,
 * which is how Claude sessions deliver live rate-limit readings the backend
 * overlays on the quota. When the backend's Claude read fails (e.g. HTTP 429)
 * it serves the last good quota flagged `claude_stale`; the bars stay, dimmed,
 * with when they were read and why they are not fresh.
 */

import { useCallback, useEffect, useRef, useState } from "react"
import { useLocale, useTranslations } from "next-intl"

import {
  Popover,
  PopoverContent,
  PopoverTrigger,
} from "@/components/ui/popover"
import { useAcpEvent } from "@/contexts/acp-connections-context"
import { getAgentLabel } from "@/lib/custom-agents"
import { formatLimitResetTime } from "@/lib/model-scorecard"
import {
  asRouterProfile,
  fetchUsageMeter,
  formatRelativeTime,
  formatTokens,
  setRouterProfile,
  usageLevel,
  ROUTER_PROFILES,
  type RouterProfile,
  type UsageLevel,
  type UsageMeterResponse,
  type UsageMeterWindow,
} from "@/lib/usage-meter"
import { cn } from "@/lib/utils"

/** Background refresh cadence — the meter is a plan quota, not a live stream. */
const POLL_MS = 2 * 60 * 1000

/** Minimum gap between `usage_update`-driven refreshes; a burst inside the gap
 *  collapses into one trailing refresh. */
const LIVE_REFRESH_MIN_MS = 15 * 1000

/** At or above this quota percent the router drops to `equilibrado` on its own
 *  (`routing::QUOTA_HIGH_PCT`), which the popover states rather than hides. */
const QUOTA_AUTO_SWITCH_PCT = 90

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
  const locale = useLocale()
  const [meter, setMeter] = useState<UsageMeterResponse | null>(null)
  // Reference instant for the relative "updated" / "retry" times; set on each
  // read and when the popover opens so render stays pure.
  const [now, setNow] = useState(0)
  // Guards against a slow poll landing after unmount or overwriting a fresher
  // result (same shape as `status-bar-mcp.tsx`).
  const seqRef = useRef(0)
  const aliveRef = useRef(true)
  // Throttle state of the `usage_update`-driven refresh (see `useAcpEvent`).
  const lastLiveRef = useRef(0)
  const trailingRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  const refresh = useCallback(async () => {
    const seq = ++seqRef.current
    const next = await fetchUsageMeter()
    if (!aliveRef.current || seq !== seqRef.current) return
    setMeter(next)
    setNow(Date.now())
  }, [])

  /** Apply the profile locally first, then persist and refetch. A rejected
   *  write restores the previous value, so the switch never shows a profile
   *  the router did not accept. */
  const changeProfile = useCallback(
    async (next: RouterProfile) => {
      const previous = asRouterProfile(meter?.router_profile)
      if (next === previous) return
      setMeter((current) =>
        current ? { ...current, router_profile: next } : current
      )
      try {
        await setRouterProfile(next)
      } catch {
        setMeter((current) =>
          current ? { ...current, router_profile: previous } : current
        )
        return
      }
      // The stored value is authoritative; refetch rather than assume.
      await refresh()
    },
    [meter?.router_profile, refresh]
  )

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
      if (trailingRef.current) clearTimeout(trailingRef.current)
      trailingRef.current = null
    }
  }, [refresh])

  // Live refresh: at most one per LIVE_REFRESH_MIN_MS, with a trailing call so
  // the last reading of a burst is not lost. The envelope carries no agent
  // type; the backend only records Claude readings, so other agents' updates
  // cost a throttled re-read and nothing more.
  useAcpEvent((envelope) => {
    if (envelope.type !== "usage_update" || trailingRef.current) return
    const wait = lastLiveRef.current + LIVE_REFRESH_MIN_MS - Date.now()
    const run = () => {
      trailingRef.current = null
      lastLiveRef.current = Date.now()
      void refresh()
    }
    if (wait <= 0) run()
    else trailingRef.current = setTimeout(run, wait)
  })

  const claude = meter?.claude ?? null
  const today = meter?.today ?? []
  const fiveHour = claude?.five_hour ?? null
  const week = claude?.seven_day ?? null
  const weekTokens = meter?.week_tokens ?? 0
  const weekPercent = week ? Math.round(week.percent) : null
  const profile = asRouterProfile(meter?.router_profile)
  // The router downgrades itself past 90% of either window; say so rather than
  // letting a `calidad` switch look like it is still in force.
  const quotaForcesEquilibrado =
    profile === "calidad" &&
    ((fiveHour?.percent ?? 0) >= QUOTA_AUTO_SWITCH_PCT ||
      (week?.percent ?? 0) >= QUOTA_AUTO_SWITCH_PCT)

  if (!meter || (!claude && today.length === 0)) return null

  // A stale `claude` is the backend's last good read: still shown, dimmed.
  const stale = Boolean(meter.claude_stale) && claude !== null
  // A live reading newer than the fetch is when the numbers were last current.
  const liveAt = newerIso(meter.claude_live_at, claude?.fetched_at)
  const updatedAgo =
    stale || liveAt
      ? formatRelativeTime(liveAt ?? claude?.fetched_at, now, locale)
      : null
  // Data that is not stale despite a failed read is live data: nothing to warn.
  const status =
    stale || claude === null
      ? claudeStatus(meter, claude !== null, now, locale, t)
      : null

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
    <Popover onOpenChange={(open) => open && setNow(Date.now())}>
      <PopoverTrigger asChild>
        <button
          aria-label={t("ariaLabel")}
          title={t("title")}
          className={cn(
            "flex items-center gap-1 tabular-nums transition-colors hover:opacity-80",
            claude ? LEVEL_TEXT[level] : "text-muted-foreground",
            stale && "opacity-70"
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
          <div className="space-y-1.5">
            <div
              data-testid="usage-quota-bars"
              className={cn("space-y-3", stale && "opacity-50")}
            >
              {fiveHour && <QuotaBar label={t("fiveHour")} window={fiveHour} />}
              {week && <QuotaBar label={t("week")} window={week} />}
            </div>
            {updatedAgo && (
              <p className="text-3xs text-muted-foreground">
                {t("updatedAgo", { ago: updatedAgo })}
              </p>
            )}
          </div>
        )}

        {status && (
          <div className="flex items-start justify-between gap-2">
            <p
              className={cn(
                "text-2xs break-words",
                status.tone === "error" ? "text-destructive" : "text-amber-500"
              )}
            >
              {status.text}
            </p>
            <button
              type="button"
              onClick={() => void refresh()}
              className="shrink-0 text-2xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline"
            >
              {t("retry")}
            </button>
          </div>
        )}

        <RouterProfileSwitch
          profile={profile}
          autoSwitch={quotaForcesEquilibrado}
          onChange={changeProfile}
        />

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

/** `candidate` when it is a valid ISO instant later than `base` (or `base` is
 *  absent/invalid), else `null`. */
function newerIso(
  candidate: string | null | undefined,
  base: string | null | undefined
): string | null {
  const at = candidate ? Date.parse(candidate) : NaN
  if (Number.isNaN(at)) return null
  const baseAt = base ? Date.parse(base) : NaN
  return Number.isNaN(baseAt) || at > baseAt ? (candidate ?? null) : null
}

/**
 * The Claude-read status line, or `null` when the last read succeeded.
 * Amber whenever there is something to show (stale data, or a transient
 * failure); red only for an auth failure with no quota data at all.
 */
function claudeStatus(
  meter: UsageMeterResponse,
  hasData: boolean,
  now: number,
  locale: string,
  t: ReturnType<typeof useTranslations<"Folder.statusBar.usage">>
): { tone: "error" | "warn"; text: string } | null {
  if (!meter.claude_error) return null
  const kind = meter.claude_error_kind ?? "other"
  if (kind === "rate_limited") {
    const time =
      formatRelativeTime(meter.claude_retry_at, now, locale) ?? t("retrySoon")
    return { tone: "warn", text: t("staleRateLimited", { time }) }
  }
  if (!hasData) {
    return {
      tone: kind === "auth" ? "error" : "warn",
      text: t("claudeError", { message: meter.claude_error }),
    }
  }
  return {
    tone: "warn",
    text: t(kind === "network" ? "staleNetwork" : "staleOther"),
  }
}

/**
 * The two-way router profile switch. A plain pair of radio-style buttons: the
 * whole popover is read-only elsewhere, and a dropdown would cost a second
 * popover inside a popover.
 */
function RouterProfileSwitch({
  profile,
  autoSwitch,
  onChange,
}: {
  profile: RouterProfile
  autoSwitch: boolean
  onChange: (profile: RouterProfile) => void | Promise<void>
}) {
  const t = useTranslations("Folder.statusBar.usage")
  return (
    <div className="space-y-1.5">
      <div className="text-2xs font-medium text-muted-foreground">
        {t("router")}
      </div>
      <div
        role="radiogroup"
        aria-label={t("router")}
        className="flex rounded-md bg-muted p-0.5"
      >
        {ROUTER_PROFILES.map((option) => (
          <button
            key={option}
            type="button"
            role="radio"
            aria-checked={profile === option}
            onClick={() => void onChange(option)}
            className={cn(
              "flex-1 rounded px-1.5 py-0.5 text-2xs transition-colors",
              profile === option
                ? "bg-background font-medium text-foreground shadow-sm"
                : "text-muted-foreground hover:text-foreground"
            )}
          >
            {t(option === "calidad" ? "profileCalidad" : "profileEquilibrado")}
          </button>
        ))}
      </div>
      <p className="text-3xs text-muted-foreground">
        {t(
          profile === "calidad"
            ? "profileCalidadHint"
            : "profileEquilibradoHint"
        )}
      </p>
      {autoSwitch && (
        <p className="text-3xs text-muted-foreground">{t("quotaAutoSwitch")}</p>
      )}
    </div>
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
