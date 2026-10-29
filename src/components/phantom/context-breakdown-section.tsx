"use client"

/**
 * Claude Code's `/context`, inside the composer's context-usage popover.
 *
 * Nothing here runs by itself. "Show breakdown" sends `/context` like a typed
 * command: Claude Code answers it locally, without calling the model (see
 * `phantom-context-breakdown.ts`), and the reply also shows in the chat. The
 * popover then reads the newest report from the conversation and draws it.
 * "Compact now" sends `/compact`, which DOES spend tokens — labelled as such.
 */

import { useMemo, useState, type CSSProperties } from "react"
import { useTranslations } from "next-intl"
import { Loader2, RefreshCw, Shrink } from "lucide-react"

import { Button } from "@/components/ui/button"
import { Input } from "@/components/ui/input"
import { formatTokenCount } from "@/lib/token-format"
import {
  type ContextBreakdown,
  type ContextCategory,
  autocompactThreshold,
  findLatestContextReport,
  parseContextReport,
} from "@/lib/phantom-context-breakdown"
import { useConversationRuntimeStore } from "@/stores/conversation-runtime-store"

/**
 * Categorical slots (validated light/dark pairs), assigned by category NAME in
 * a fixed order so a colour always means the same thing between reports.
 */
const SLOTS: { light: string; dark: string }[] = [
  { light: "#2a78d6", dark: "#3987e5" },
  { light: "#eb6834", dark: "#d95926" },
  { light: "#1baf7a", dark: "#199e70" },
  { light: "#eda100", dark: "#c98500" },
  { light: "#e87ba4", dark: "#d55181" },
  { light: "#008300", dark: "#008300" },
  { light: "#6250d6", dark: "#9085e9" },
  { light: "#e34948", dark: "#e66767" },
]
const SLOT_ORDER = [
  "system prompt",
  "system tools",
  "mcp tools",
  "custom agents",
  "memory files",
  "skills",
  "messages",
]

export function categorySlot(name: string): number {
  const i = SLOT_ORDER.indexOf(name.toLowerCase())
  return i >= 0 ? i : SLOTS.length - 1
}

const HATCH =
  "repeating-linear-gradient(135deg, currentColor 0 1px, transparent 1px 4px)"

function swatchStyle(category: ContextCategory): CSSProperties | undefined {
  if (category.kind !== "used") return undefined
  const slot = SLOTS[categorySlot(category.name)]
  return { "--seg": slot.light, "--seg-dark": slot.dark } as CSSProperties
}

function Swatch({ category }: { category: ContextCategory }) {
  if (category.kind === "buffer") {
    return (
      <span
        aria-hidden
        className="inline-block size-2.5 shrink-0 rounded-[3px] text-muted-foreground/70 ring-1 ring-border"
        style={{ backgroundImage: HATCH }}
      />
    )
  }
  if (category.kind !== "used") {
    return (
      <span
        aria-hidden
        className="inline-block size-2.5 shrink-0 rounded-[3px] bg-muted ring-1 ring-border"
      />
    )
  }
  return (
    <span
      aria-hidden
      className="inline-block size-2.5 shrink-0 rounded-[3px] bg-[var(--seg)] dark:bg-[var(--seg-dark)]"
      style={swatchStyle(category)}
    />
  )
}

function pct(tokens: number, of: number): string {
  return `${((tokens / of) * 100).toFixed(1)}%`
}

export function ContextBreakdownView({
  breakdown,
  windowSize,
}: {
  breakdown: ContextBreakdown
  windowSize: number | null
}) {
  const t = useTranslations("Phantom.contextBreakdown")
  const max = breakdown.maxTokens ?? windowSize
  const inWindow = breakdown.categories.filter(
    (c) => c.kind === "used" || c.kind === "buffer"
  )
  const threshold = autocompactThreshold(breakdown, windowSize)

  return (
    <div className="space-y-2">
      {max != null && max > 0 ? (
        <div
          role="img"
          aria-label={t("barAria")}
          className="flex h-2.5 w-full gap-[2px] overflow-hidden rounded-full bg-muted"
        >
          {inWindow.map((c) => (
            <div
              key={c.name}
              data-testid="context-segment"
              title={`${c.name}: ${formatTokenCount(c.tokens)} (${pct(c.tokens, max)})`}
              className={
                c.kind === "buffer"
                  ? "h-full shrink-0 text-muted-foreground/60"
                  : "h-full shrink-0 bg-[var(--seg)] dark:bg-[var(--seg-dark)]"
              }
              style={{
                width: `${Math.min(100, (c.tokens / max) * 100)}%`,
                ...(c.kind === "buffer"
                  ? { backgroundImage: HATCH }
                  : swatchStyle(c)),
              }}
            />
          ))}
        </div>
      ) : null}

      <table className="w-full text-xs">
        <thead className="sr-only">
          <tr>
            <th>{t("category")}</th>
            <th>{t("tokens")}</th>
            <th>{t("share")}</th>
          </tr>
        </thead>
        <tbody>
          {breakdown.categories.map((c) => (
            <tr
              key={c.name}
              className={c.kind === "deferred" ? "text-muted-foreground" : ""}
            >
              <td className="py-0.5 pr-2">
                <span className="flex items-center gap-1.5">
                  <Swatch category={c} />
                  <span className="truncate">{c.name}</span>
                </span>
              </td>
              <td className="py-0.5 text-right tabular-nums">
                {formatTokenCount(c.tokens)}
              </td>
              <td className="w-12 py-0.5 text-right tabular-nums text-muted-foreground">
                {c.percent != null
                  ? `${c.percent}%`
                  : max
                    ? pct(c.tokens, max)
                    : ""}
              </td>
            </tr>
          ))}
        </tbody>
      </table>

      {threshold != null ? (
        <div className="flex items-center justify-between text-xs text-muted-foreground">
          <span>{t("autocompactAt")}</span>
          <span className="tabular-nums">{formatTokenCount(threshold)}</span>
        </div>
      ) : null}

      {breakdown.memoryFiles.length > 0 ? (
        <div className="space-y-0.5">
          <div className="text-xs font-medium">{t("memoryFiles")}</div>
          {breakdown.memoryFiles.map((f) => (
            <div
              key={f.path}
              className="flex items-center justify-between gap-2 text-[11px] text-muted-foreground"
            >
              <span className="truncate font-mono" title={f.path}>
                {f.path}
              </span>
              <span className="shrink-0 tabular-nums">
                {formatTokenCount(f.tokens)}
              </span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  )
}

export function ContextBreakdownSection({
  conversationId,
  windowSize,
  isPrompting,
  onSendCommand,
}: {
  conversationId: number | null
  windowSize: number | null
  isPrompting: boolean
  onSendCommand: (text: string) => void
}) {
  const t = useTranslations("Phantom.contextBreakdown")
  const markdown = useConversationRuntimeStore((s) =>
    conversationId == null
      ? null
      : (findLatestContextReport(s.byConversationId.get(conversationId))
          ?.markdown ?? null)
  )
  const breakdown = useMemo(
    () => (markdown ? parseContextReport(markdown) : null),
    [markdown]
  )
  // The report present when "show" was clicked; waiting ends when it changes.
  const [requestedFrom, setRequestedFrom] = useState<string | null | undefined>(
    undefined
  )
  // An identical report (nothing changed since) must still end the wait: the
  // turn starting and then finishing counts as the answer having arrived.
  const [promptSeen, setPromptSeen] = useState(false)
  if (requestedFrom !== undefined && isPrompting && !promptSeen) {
    setPromptSeen(true)
  }
  const waiting =
    requestedFrom !== undefined &&
    requestedFrom === markdown &&
    !(promptSeen && !isPrompting)
  const [instructions, setInstructions] = useState("")

  const requestReport = () => {
    setRequestedFrom(markdown)
    setPromptSeen(false)
    onSendCommand("/context")
  }

  return (
    <div className="space-y-2 border-t border-border pt-2">
      <div className="flex items-center justify-between gap-2">
        <span className="text-xs font-medium">{t("title")}</span>
        {breakdown ? (
          <Button
            type="button"
            variant="ghost"
            size="icon-xs"
            disabled={isPrompting || waiting}
            onClick={requestReport}
            title={t("refresh")}
            aria-label={t("refresh")}
          >
            {waiting ? (
              <Loader2 className="size-3.5 animate-spin" />
            ) : (
              <RefreshCw className="size-3.5" />
            )}
          </Button>
        ) : null}
      </div>

      {breakdown ? (
        <ContextBreakdownView breakdown={breakdown} windowSize={windowSize} />
      ) : (
        <div className="space-y-1.5">
          <p className="text-[11px] leading-4 text-muted-foreground">
            {t("explain")}
          </p>
          <Button
            type="button"
            size="sm"
            variant="outline"
            className="w-full"
            disabled={isPrompting || waiting}
            onClick={requestReport}
          >
            {waiting ? <Loader2 className="size-3.5 animate-spin" /> : null}
            {t("show")}
          </Button>
        </div>
      )}
      <p className="text-[10px] leading-3.5 text-muted-foreground">
        {t("zeroTokens")}
      </p>

      <div className="space-y-1.5 border-t border-border pt-2">
        <Input
          value={instructions}
          onChange={(e) => setInstructions(e.target.value)}
          placeholder={t("compactPlaceholder")}
          aria-label={t("compactPlaceholder")}
          className="h-7 text-xs"
          disabled={isPrompting}
        />
        <Button
          type="button"
          size="sm"
          variant="secondary"
          className="w-full"
          disabled={isPrompting}
          onClick={() => {
            const extra = instructions.replace(/\s+/g, " ").trim()
            onSendCommand(extra ? `/compact ${extra}` : "/compact")
            setInstructions("")
          }}
        >
          <Shrink className="size-3.5" />
          {t("compact")}
        </Button>
        <p className="text-[10px] leading-3.5 text-muted-foreground">
          {isPrompting ? t("busy") : t("compactCost")}
        </p>
      </div>
    </div>
  )
}
