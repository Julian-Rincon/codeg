"use client"

/**
 * The model-quota-failover handoff card: shown inline above the composer
 * (see `ConversationShell`'s `handoffCard` slot) when the active agent/model
 * has hit its usage limit — offers the best MEASURED successor and, on
 * acceptance, hands the conversation off with full context.
 *
 * Not a blocking modal: the user can keep reading the transcript, dismiss it
 * ("Esperar"), or act. Driven entirely by `useFailoverOffer` — this component
 * only renders whatever state that hook is in.
 */

import { useState } from "react"
import { useLocale, useTranslations } from "next-intl"
import { AlertTriangle, Loader2 } from "lucide-react"

import { AgentIcon } from "@/components/agent-icon"
import { Button } from "@/components/ui/button"
import { getAgentLabel } from "@/lib/custom-agents"
import { formatLimitResetTime } from "@/lib/model-scorecard"
import { cn } from "@/lib/utils"
import type {
  PhantomSuccessorCandidate,
  PhantomSuccessorResponse,
} from "@/lib/model-failover"
import type { AgentType } from "@/lib/types"

export interface ModelFailoverCardProps {
  /** The agent that just ran out of tokens — what the title names. */
  limitedAgentType: AgentType
  /** Adapter-authored wording of the failure (e.g. "You've hit your session
   *  limit"), shown as a supporting line when it says more than the title. */
  reasonText: string
  /** `null` while the successor lookup is still in flight (`loading`). */
  offer: PhantomSuccessorResponse | null
  loading: boolean
  accepting: boolean
  /** The last handoff attempt's error, or `null`. */
  error: string | null
  onAccept: (
    candidate?: Pick<PhantomSuccessorCandidate, "agent_type" | "model">
  ) => void
  /** "Esperar" — closes the card for this hit; a later, distinct hit re-offers. */
  onDismiss: () => void
  onRetry: () => void
}

export function ModelFailoverCard({
  limitedAgentType,
  reasonText,
  offer,
  loading,
  accepting,
  error,
  onAccept,
  onDismiss,
  onRetry,
}: ModelFailoverCardProps) {
  const t = useTranslations("Folder.chat.modelFailover")
  const locale = useLocale()
  const [runnerUpOpen, setRunnerUpOpen] = useState(false)

  const resetHint =
    offer?.limited?.resets_hint?.trim() ||
    formatLimitResetTime(offer?.limited?.resets_at, locale) ||
    null
  const agentLabel = getAgentLabel(limitedAgentType)
  const title = resetHint
    ? t("titleWithReset", { agent: agentLabel, resets: resetHint })
    : t("title", { agent: agentLabel })
  const successor = offer?.successor ?? null
  const runnerUp = offer?.runner_up ?? null
  const locked = accepting

  return (
    <div
      role="status"
      aria-live="polite"
      className="w-full space-y-3 rounded-lg border border-amber-500/30 bg-amber-500/5 p-4"
    >
      <div className="flex items-start gap-2">
        <AlertTriangle
          aria-hidden
          className="mt-0.5 size-4 shrink-0 text-amber-600 dark:text-amber-400"
        />
        <div className="min-w-0 flex-1 space-y-0.5">
          <p className="text-sm font-medium text-foreground">{title}</p>
          {reasonText.trim() && reasonText.trim() !== title && (
            <p
              className="truncate text-xs text-muted-foreground"
              title={reasonText}
            >
              {reasonText}
            </p>
          )}
        </div>
      </div>

      {loading ? (
        <div className="flex items-center gap-2 text-xs text-muted-foreground">
          <Loader2
            aria-hidden
            className="size-3.5 shrink-0 animate-spin motion-reduce:animate-none"
          />
          {t("looking")}
        </div>
      ) : successor ? (
        <>
          <SuccessorChip candidate={successor} />
          <div className="flex flex-wrap items-center gap-2">
            <Button
              size="sm"
              disabled={locked}
              onClick={() => onAccept()}
              aria-label={t("continueWith", {
                agent: getAgentLabel(successor.agent_type),
                model: successorLabel(successor),
              })}
            >
              {accepting && (
                <Loader2
                  aria-hidden
                  className="size-3.5 animate-spin motion-reduce:animate-none"
                />
              )}
              {accepting
                ? t("accepting")
                : t("continueWith", {
                    agent: getAgentLabel(successor.agent_type),
                    model: successorLabel(successor),
                  })}
            </Button>
            <Button
              variant="outline"
              size="sm"
              disabled={locked}
              onClick={onDismiss}
            >
              {t("wait")}
            </Button>
            {runnerUp && !runnerUpOpen && (
              <button
                type="button"
                disabled={locked}
                onClick={() => setRunnerUpOpen(true)}
                className="text-2xs text-muted-foreground underline-offset-2 hover:text-foreground hover:underline disabled:opacity-50"
              >
                {t("chooseAnother")}
              </button>
            )}
          </div>
          {runnerUp && runnerUpOpen && (
            <div className="flex flex-wrap items-center gap-2 rounded-md border border-border/50 bg-card/60 p-2">
              <span className="text-3xs text-muted-foreground">
                {t("runnerUpLabel", {
                  agent: getAgentLabel(runnerUp.agent_type),
                  model: successorLabel(runnerUp),
                })}
              </span>
              <Button
                variant="outline"
                size="sm"
                disabled={locked}
                onClick={() =>
                  onAccept({
                    agent_type: runnerUp.agent_type,
                    model: runnerUp.model,
                  })
                }
              >
                {t("useRunnerUp")}
              </Button>
            </div>
          )}
        </>
      ) : (
        <div className="flex flex-wrap items-center justify-between gap-2">
          <p className="text-xs text-muted-foreground">
            {resetHint
              ? t("noSuccessorWithReset", { resets: resetHint })
              : t("noSuccessor")}
          </p>
          <Button variant="outline" size="sm" onClick={onDismiss}>
            {t("wait")}
          </Button>
        </div>
      )}

      {error && (
        <div className="flex flex-wrap items-center gap-2 text-xs text-destructive">
          <span>{t("error")}</span>
          <button
            type="button"
            onClick={onRetry}
            className="underline underline-offset-2 hover:no-underline"
          >
            {t("retry")}
          </button>
        </div>
      )}
    </div>
  )
}

function successorLabel(candidate: PhantomSuccessorCandidate): string {
  return candidate.label ?? candidate.model ?? candidate.agent_type
}

function SuccessorChip({
  candidate,
}: {
  candidate: PhantomSuccessorCandidate
}) {
  return (
    <div
      className={cn(
        "flex min-w-0 items-center gap-2 rounded-md border border-border/60",
        "bg-card/80 px-2.5 py-1.5"
      )}
    >
      <AgentIcon agentType={candidate.agent_type} className="size-4 shrink-0" />
      <div className="min-w-0">
        <p className="truncate text-xs font-medium text-foreground">
          {getAgentLabel(candidate.agent_type)} · {successorLabel(candidate)}
        </p>
        {candidate.reason?.trim() && (
          <p className="truncate text-3xs text-muted-foreground">
            {candidate.reason}
          </p>
        )}
      </div>
    </div>
  )
}
