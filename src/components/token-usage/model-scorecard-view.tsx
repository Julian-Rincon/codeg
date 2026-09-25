"use client"

import { useMemo, useState } from "react"
import { useLocale, useTranslations } from "next-intl"
import { ArrowDown, ArrowUp, ChartNoAxesColumn, RefreshCw } from "lucide-react"
import { toast } from "sonner"
import { Badge } from "@/components/ui/badge"
import { Button } from "@/components/ui/button"
import { tokenUsageSync } from "@/lib/api"
import { toErrorMessage } from "@/lib/app-error"
import { getAgentLabel } from "@/lib/custom-agents"
import {
  formatScorecardContext,
  formatScorecardPercent,
  formatScorecardSeconds,
  formatScorecardTokensPerSecond,
  hasScorecardData,
  scorecardStrengthLabels,
  useModelScorecard,
  type AnyTranslate,
  type ModelScorecardEntry,
} from "@/lib/model-scorecard"
import { cn } from "@/lib/utils"

type SortKey =
  | "agent"
  | "model"
  | "conversations"
  | "turns"
  | "avg_turn_ms"
  | "output_tokens_per_s"
  | "cache_hit_pct"
  | "tool_error_pct"
  | "context"
  | "last_used_at"

const SORT_ACCESSORS: Record<
  SortKey,
  (entry: ModelScorecardEntry) => number | string
> = {
  agent: (e) => getAgentLabel(e.agent_type),
  model: (e) => e.label || e.model,
  conversations: (e) => e.conversations,
  turns: (e) => e.turns,
  avg_turn_ms: (e) => e.avg_turn_ms ?? -1,
  output_tokens_per_s: (e) => e.output_tokens_per_s ?? -1,
  cache_hit_pct: (e) => e.cache_hit_pct ?? -1,
  tool_error_pct: (e) => e.tool_error_pct ?? -1,
  context: (e) => e.spec?.context ?? -1,
  last_used_at: (e) => e.last_used_at ?? "",
}

/** One sortable column header. Mirrors the ranked-bar sort affordance the
 *  rest of the dashboard uses — a plain button, arrow flips on click. */
function SortHeader({
  label,
  sortKey,
  active,
  dir,
  onClick,
  align = "start",
}: {
  label: string
  sortKey: SortKey
  active: boolean
  dir: "asc" | "desc"
  onClick: (key: SortKey) => void
  align?: "start" | "end"
}) {
  const Icon = dir === "asc" ? ArrowUp : ArrowDown
  return (
    <th
      scope="col"
      className={cn(
        "px-3 py-2 text-xs font-medium text-muted-foreground",
        align === "end" ? "text-end" : "text-start"
      )}
    >
      <button
        type="button"
        onClick={() => onClick(sortKey)}
        className={cn(
          "inline-flex items-center gap-1 hover:text-foreground",
          active && "text-foreground"
        )}
      >
        {label}
        {active && <Icon className="size-3" aria-hidden="true" />}
      </button>
    </th>
  )
}

/** Muted "not enough data" cell — never a number computed on too few turns. */
function NoDataCell({ label }: { label: string }) {
  return <span className="text-muted-foreground/70">{label}</span>
}

/**
 * The Modelos tab's full scorecard table: every measured model, sortable,
 * with the same "never fabricate a number" discipline as the picker badges —
 * a model with too few turns shows the localized no-data note instead of any
 * of its averages. Read-only: nothing here changes a session's model.
 */
export function ModelScorecardView() {
  const t = useTranslations("TokenUsage")
  const tScorecard = useTranslations("ModelScorecard") as AnyTranslate
  const locale = useLocale()
  const { scorecard, loading, refresh } = useModelScorecard()
  const [syncing, setSyncing] = useState(false)
  const [sortKey, setSortKey] = useState<SortKey>("turns")
  const [sortDir, setSortDir] = useState<"asc" | "desc">("desc")

  const handleSort = (key: SortKey) => {
    if (key === sortKey) {
      setSortDir((d) => (d === "asc" ? "desc" : "asc"))
    } else {
      setSortKey(key)
      setSortDir("desc")
    }
  }

  const rows = useMemo(() => {
    const models = scorecard?.models ?? []
    const accessor = SORT_ACCESSORS[sortKey]
    const sorted = [...models].sort((a, b) => {
      const av = accessor(a)
      const bv = accessor(b)
      const cmp =
        typeof av === "string" && typeof bv === "string"
          ? av.localeCompare(bv)
          : Number(av) - Number(bv)
      return sortDir === "asc" ? cmp : -cmp
    })
    return sorted
  }, [scorecard, sortKey, sortDir])

  const handleSync = async () => {
    setSyncing(true)
    try {
      await tokenUsageSync("incremental")
      await refresh()
    } catch (e) {
      toast.error(t("syncFailed"), { description: toErrorMessage(e) })
    } finally {
      setSyncing(false)
    }
  }

  const bestFor = scorecard?.best_for ?? []

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="shrink-0 p-4">
        <div className="mx-auto flex w-full max-w-6xl flex-wrap items-center justify-between gap-2">
          <div className="min-w-0">
            <p className="text-xs text-muted-foreground">
              {t("modelsExplain")}
            </p>
            {scorecard && (
              <p className="mt-0.5 text-3xs text-muted-foreground/70">
                {t("modelsGeneratedAt", {
                  time: new Date(scorecard.generated_at).toLocaleString(locale),
                })}
              </p>
            )}
          </div>
          <Button
            type="button"
            size="sm"
            variant="outline"
            className="gap-1.5"
            disabled={syncing}
            onClick={() => void handleSync()}
          >
            <RefreshCw className={cn("size-3.5", syncing && "animate-spin")} />
            {t("modelsSyncAction")}
          </Button>
        </div>
      </div>

      <div className="min-h-0 flex-1 overflow-auto px-4 pb-4">
        <div className="mx-auto w-full max-w-6xl space-y-4">
          {bestFor.length > 0 && (
            <section className="rounded-xl border border-border bg-card p-4">
              <h2 className="mb-2 flex items-center gap-1.5 text-[0.8125rem] font-semibold">
                <ChartNoAxesColumn
                  className="size-3.5 text-muted-foreground"
                  aria-hidden="true"
                />
                {t("modelsBestForTitle")}
              </h2>
              <div className="flex flex-wrap gap-1.5">
                {bestFor.map((win) => {
                  const modelEntry = scorecard?.models.find(
                    (m) =>
                      m.agent_type === win.agent_type && m.model === win.model
                  )
                  const modelLabel = modelEntry?.label || win.model
                  const categoryLabel = tScorecard(
                    `category${win.category
                      .split("_")
                      .map((s) => s[0].toUpperCase() + s.slice(1))
                      .join("")}`
                  )
                  return (
                    <Badge
                      key={win.category}
                      variant="outline"
                      className="gap-1 px-2 py-1 text-xs font-normal"
                      title={
                        win.runner_up
                          ? t("modelsRunnerUp", {
                              agent: getAgentLabel(win.runner_up.agent_type),
                              model: win.runner_up.model,
                              value: win.runner_up.value,
                            })
                          : undefined
                      }
                    >
                      <span className="font-medium">{categoryLabel}</span>
                      <span className="text-muted-foreground">
                        {getAgentLabel(win.agent_type)} · {modelLabel}
                      </span>
                    </Badge>
                  )
                })}
              </div>
            </section>
          )}

          <section className="overflow-hidden rounded-xl border border-border bg-card">
            {loading && !scorecard ? (
              <div
                className="h-40 animate-pulse bg-muted/40"
                aria-hidden="true"
              />
            ) : rows.length === 0 ? (
              <div className="flex flex-col items-center gap-2 p-8 text-center">
                <ChartNoAxesColumn
                  className="size-8 text-muted-foreground/40"
                  aria-hidden="true"
                />
                <p className="text-sm text-muted-foreground">
                  {t("modelsEmpty")}
                </p>
              </div>
            ) : (
              <table className="w-full border-collapse text-sm">
                <thead className="border-b border-border">
                  <tr>
                    <SortHeader
                      label={t("modelsColAgent")}
                      sortKey="agent"
                      active={sortKey === "agent"}
                      dir={sortDir}
                      onClick={handleSort}
                    />
                    <SortHeader
                      label={t("modelsColModel")}
                      sortKey="model"
                      active={sortKey === "model"}
                      dir={sortDir}
                      onClick={handleSort}
                    />
                    <SortHeader
                      label={t("modelsColConversations")}
                      sortKey="conversations"
                      active={sortKey === "conversations"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <SortHeader
                      label={t("modelsColTurns")}
                      sortKey="turns"
                      active={sortKey === "turns"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <SortHeader
                      label={t("modelsColAvgTurn")}
                      sortKey="avg_turn_ms"
                      active={sortKey === "avg_turn_ms"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <SortHeader
                      label={t("modelsColTokPerSec")}
                      sortKey="output_tokens_per_s"
                      active={sortKey === "output_tokens_per_s"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <SortHeader
                      label={t("modelsColCacheHit")}
                      sortKey="cache_hit_pct"
                      active={sortKey === "cache_hit_pct"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <SortHeader
                      label={t("modelsColToolErrors")}
                      sortKey="tool_error_pct"
                      active={sortKey === "tool_error_pct"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <SortHeader
                      label={t("modelsColContext")}
                      sortKey="context"
                      active={sortKey === "context"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <SortHeader
                      label={t("modelsColLastUsed")}
                      sortKey="last_used_at"
                      active={sortKey === "last_used_at"}
                      dir={sortDir}
                      onClick={handleSort}
                      align="end"
                    />
                    <th
                      scope="col"
                      className="px-3 py-2 text-start text-xs font-medium text-muted-foreground"
                    >
                      {t("modelsColStrengths")}
                    </th>
                  </tr>
                </thead>
                <tbody>
                  {rows.map((entry) => {
                    const measured = hasScorecardData(entry)
                    const duration = formatScorecardSeconds(
                      entry.avg_turn_ms,
                      locale
                    )
                    const tps = formatScorecardTokensPerSecond(
                      entry.output_tokens_per_s,
                      locale
                    )
                    const cacheHit = formatScorecardPercent(
                      entry.cache_hit_pct,
                      locale
                    )
                    const errorPct = formatScorecardPercent(
                      entry.tool_error_pct,
                      locale
                    )
                    const context = formatScorecardContext(
                      entry.spec?.context ?? null
                    )
                    const badges = measured
                      ? scorecardStrengthLabels(entry, tScorecard)
                      : []
                    return (
                      <tr
                        key={`${entry.agent_type}:${entry.model}`}
                        className="border-b border-border/60 last:border-0"
                      >
                        <td className="px-3 py-2 text-muted-foreground">
                          {getAgentLabel(entry.agent_type)}
                        </td>
                        <td className="px-3 py-2 font-medium">
                          <div className="flex items-center gap-1.5">
                            <span>{entry.label || entry.model}</span>
                            {entry.available === false && (
                              <Badge
                                variant="outline"
                                className="px-1 text-3xs font-normal text-muted-foreground"
                              >
                                {tScorecard("unavailable")}
                              </Badge>
                            )}
                          </div>
                        </td>
                        <td className="px-3 py-2 text-end tabular-nums">
                          {entry.conversations.toLocaleString(locale)}
                        </td>
                        <td className="px-3 py-2 text-end tabular-nums">
                          {entry.turns.toLocaleString(locale)}
                        </td>
                        <td className="px-3 py-2 text-end tabular-nums">
                          {!measured || !duration ? (
                            <NoDataCell label={tScorecard("noData")} />
                          ) : (
                            tScorecard("metricDuration", { value: duration })
                          )}
                        </td>
                        <td className="px-3 py-2 text-end tabular-nums">
                          {!measured || !tps ? (
                            <NoDataCell label={tScorecard("noData")} />
                          ) : (
                            tScorecard("metricTokPerSec", { value: tps })
                          )}
                        </td>
                        <td className="px-3 py-2 text-end tabular-nums">
                          {!measured || !cacheHit ? (
                            <NoDataCell label="—" />
                          ) : (
                            `${cacheHit}%`
                          )}
                        </td>
                        <td className="px-3 py-2 text-end tabular-nums">
                          {!measured || !errorPct ? (
                            <NoDataCell label="—" />
                          ) : (
                            `${errorPct}%`
                          )}
                        </td>
                        <td className="px-3 py-2 text-end tabular-nums">
                          {context ?? <NoDataCell label="—" />}
                        </td>
                        <td className="px-3 py-2 text-end text-muted-foreground">
                          {entry.last_used_at
                            ? new Date(entry.last_used_at).toLocaleDateString(
                                locale
                              )
                            : "—"}
                        </td>
                        <td className="px-3 py-2">
                          {badges.length > 0 ? (
                            <div className="flex flex-wrap gap-1">
                              {badges.map((badge) => (
                                <Badge
                                  key={badge}
                                  variant="outline"
                                  className="px-1 text-3xs font-normal text-[var(--phantom-accent)]"
                                >
                                  {badge}
                                </Badge>
                              ))}
                            </div>
                          ) : (
                            <NoDataCell label={tScorecard("noData")} />
                          )}
                        </td>
                      </tr>
                    )
                  })}
                </tbody>
              </table>
            )}
          </section>
        </div>
      </div>
    </div>
  )
}
