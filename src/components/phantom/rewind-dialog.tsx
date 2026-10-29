"use client"

/**
 * "Rebobinar aquí": the hover action on a user message and the dialog it
 * opens — restore the code and/or the conversation to just before that
 * message. Backend: `src-tauri/src/phantom/checkpoints`. No model calls.
 */

import { memo, useCallback, useEffect, useMemo, useState } from "react"
import { useTranslations } from "next-intl"
import { History, Loader2 } from "lucide-react"

import { MessageAction } from "@/components/ai-elements/message"
import { Button } from "@/components/ui/button"
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { notify } from "@/lib/notify"
import {
  fetchCheckpointPreview,
  isCheckpointBusyError,
  planConversationRewind,
  restoreCheckpoint,
  summarizeChangedFiles,
  type CheckpointPreview,
  type PhantomRewindContext,
} from "@/lib/phantom-checkpoints"
import type { MessageTurn } from "@/lib/types"
import { cn } from "@/lib/utils"

type RewindChoice = "both" | "conversation" | "code"

function errorText(err: unknown): string {
  if (err instanceof Error) return err.message
  if (err && typeof err === "object" && "message" in err) {
    const message = (err as { message?: unknown }).message
    if (typeof message === "string") return message
  }
  return String(err)
}

export interface RewindDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  context: PhantomRewindContext
  /** The rendered thread item of the user message. */
  itemKey: string
  /** The message's own turn (id + timestamp locate its checkpoint). */
  turn: MessageTurn | undefined
  /** The message's text, as the composer should get it back. */
  text: string
  /** A turn is running on this conversation right now. */
  busy: boolean
}

export function RewindDialog({
  open,
  onOpenChange,
  context,
  itemKey,
  turn,
  text,
  busy,
}: RewindDialogProps) {
  const t = useTranslations("Phantom.rewind")
  const [preview, setPreview] = useState<CheckpointPreview | null>(null)
  const [previewError, setPreviewError] = useState<string | null>(null)
  const [choice, setChoice] = useState<RewindChoice | null>(null)
  const [running, setRunning] = useState(false)
  const [runError, setRunError] = useState<string | null>(null)

  const { conversationId, canFork, resolveForkTarget } = context
  const plan = useMemo(
    () => planConversationRewind(resolveForkTarget(itemKey), canFork),
    [canFork, itemKey, resolveForkTarget]
  )

  const turnId = turn?.id ?? null
  const turnTimestamp = turn?.timestamp ?? null
  useEffect(() => {
    if (!open) return
    let cancelled = false
    setPreview(null)
    setPreviewError(null)
    fetchCheckpointPreview({
      conversationId,
      messageId: turnId,
      turnTimestamp,
      turnText: text || null,
    })
      .then((result) => {
        if (!cancelled) setPreview(result)
      })
      .catch((err: unknown) => {
        if (!cancelled) setPreviewError(errorText(err))
      })
    return () => {
      cancelled = true
    }
  }, [conversationId, open, text, turnId, turnTimestamp])

  const codeReady = preview?.status === "ready" && preview.checkpoint != null
  const conversationReady = plan.mode !== "blocked"
  const isBusy = busy || preview?.busy === true
  const available: Record<RewindChoice, boolean> = {
    both: codeReady && conversationReady,
    conversation: conversationReady,
    code: codeReady,
  }
  const fallbackChoice: RewindChoice | null = available.both
    ? "both"
    : available.conversation
      ? "conversation"
      : available.code
        ? "code"
        : null
  // The user's pick, unless the preview since made it impossible.
  const selected = choice && available[choice] ? choice : fallbackChoice

  const undoRestore = useCallback(
    async (safetyCheckpointId: number) => {
      try {
        await restoreCheckpoint(conversationId, safetyCheckpointId)
        notify({
          level: "info",
          key: `phantom-rewind:${conversationId}`,
          title: t("undone"),
        })
      } catch (err) {
        notify({
          level: "error",
          key: `phantom-rewind:${conversationId}`,
          title: isCheckpointBusyError(err)
            ? t("busy")
            : t("undoFailed", { error: errorText(err) }),
        })
      }
    },
    [conversationId, t]
  )

  const handleConfirm = useCallback(async () => {
    if (!selected || isBusy || running) return
    setRunning(true)
    setRunError(null)
    try {
      if (selected !== "conversation" && preview?.checkpoint) {
        const result = await restoreCheckpoint(
          conversationId,
          preview.checkpoint.id
        )
        notify({
          level: "info",
          key: `phantom-rewind:${conversationId}`,
          title: t("codeRestored"),
          description: t("codeRestoredDetail", {
            written: result.written,
            deleted: result.deleted,
          }),
          actions: [
            {
              label: t("undo"),
              onClick: () => void undoRestore(result.safetyCheckpointId),
            },
          ],
        })
      }
      if (selected !== "code" && plan.mode !== "blocked") {
        const ok = await context.rewindConversation(plan, text)
        if (!ok) {
          setRunError(t("conversationFailed"))
          return
        }
      }
      onOpenChange(false)
    } catch (err) {
      setRunError(
        isCheckpointBusyError(err)
          ? t("busy")
          : t("restoreFailed", { error: errorText(err) })
      )
    } finally {
      setRunning(false)
    }
  }, [
    context,
    conversationId,
    isBusy,
    onOpenChange,
    plan,
    preview,
    running,
    selected,
    t,
    text,
    undoRestore,
  ])

  const codeNote = previewError
    ? t("previewFailed", { error: previewError })
    : preview == null
      ? t("loadingPreview")
      : preview.status === "not_git"
        ? t("codeNotGit")
        : preview.status === "no_checkpoint"
          ? t("codeNoCheckpoint")
          : preview.status === "skipped"
            ? t("codeSkipped")
            : preview.files.length === 0
              ? t("codeClean")
              : null

  const conversationNote =
    plan.mode === "fork"
      ? t("conversationFork")
      : plan.mode === "blocked"
        ? t("conversationUnloaded")
        : plan.reason === "first"
          ? t("conversationFreshFirst")
          : plan.reason === "unnamed"
            ? t("conversationFreshUnnamed")
            : t("conversationFreshUnsupported")

  const summary = preview
    ? summarizeChangedFiles(preview.files, preview.moreFiles)
    : null

  const options: { value: RewindChoice; label: string }[] = [
    { value: "both", label: t("optionBoth") },
    { value: "conversation", label: t("optionConversation") },
    { value: "code", label: t("optionCode") },
  ]

  return (
    <Dialog open={open} onOpenChange={running ? undefined : onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle>{t("title")}</DialogTitle>
          <DialogDescription>{t("description")}</DialogDescription>
        </DialogHeader>

        <div
          role="radiogroup"
          aria-label={t("title")}
          className="flex flex-col gap-1.5"
        >
          {options.map((option) => {
            const disabled = !available[option.value] || isBusy || running
            const checked = selected === option.value
            return (
              <button
                key={option.value}
                type="button"
                role="radio"
                aria-checked={checked}
                disabled={disabled}
                onClick={() => setChoice(option.value)}
                className={cn(
                  "rounded-md border px-3 py-2 text-left text-sm transition-colors",
                  checked
                    ? "border-primary bg-primary/5"
                    : "border-border hover:bg-muted/50",
                  disabled &&
                    "cursor-not-allowed opacity-50 hover:bg-transparent"
                )}
              >
                {option.label}
              </button>
            )
          })}
        </div>

        <div className="space-y-2 text-xs text-muted-foreground">
          <p>{conversationNote}</p>
          {codeNote ? (
            <p>{codeNote}</p>
          ) : summary && preview ? (
            <div className="space-y-1">
              <p>
                {t("filesIntro")}{" "}
                <span className="font-medium text-foreground">
                  {t("filesSummary", {
                    files: summary.files,
                    additions: summary.additions,
                    deletions: summary.deletions,
                  })}
                </span>
              </p>
              <ul
                aria-label={t("filesIntro")}
                className="max-h-48 overflow-y-auto rounded-md border bg-muted/30 p-2 font-mono text-[0.6875rem]"
              >
                {preview.files.map((file) => (
                  <li key={file.path} className="flex items-center gap-2">
                    <span
                      title={t(`file_${file.status}`)}
                      className={cn(
                        "w-3 shrink-0 font-semibold",
                        file.status === "added" && "text-green-600",
                        file.status === "deleted" && "text-red-600",
                        file.status === "modified" && "text-amber-600"
                      )}
                    >
                      {file.status === "added"
                        ? "A"
                        : file.status === "deleted"
                          ? "D"
                          : "M"}
                    </span>
                    <span className="min-w-0 flex-1 truncate">{file.path}</span>
                    {file.additions != null && file.deletions != null ? (
                      <span className="shrink-0">
                        <span className="text-green-600">
                          +{file.additions}
                        </span>{" "}
                        <span className="text-red-600">−{file.deletions}</span>
                      </span>
                    ) : null}
                  </li>
                ))}
                {preview.moreFiles > 0 ? (
                  <li>{t("moreFiles", { count: preview.moreFiles })}</li>
                ) : null}
              </ul>
            </div>
          ) : null}
          {isBusy ? (
            <p role="alert" className="text-amber-600">
              {t("busy")}
            </p>
          ) : null}
          {runError ? (
            <p role="alert" className="text-destructive">
              {runError}
            </p>
          ) : null}
        </div>

        <DialogFooter>
          <Button
            variant="outline"
            disabled={running}
            onClick={() => onOpenChange(false)}
          >
            {t("cancel")}
          </Button>
          <Button
            disabled={!selected || isBusy || running}
            onClick={() => void handleConfirm()}
          >
            {running ? <Loader2 className="size-4 animate-spin" /> : null}
            {running ? t("running") : t("confirm")}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/** The hover action on a user message bubble. */
export const PhantomRewindButton = memo(function PhantomRewindButton({
  context,
  itemKey,
  turn,
  getText,
  busy,
}: {
  context: PhantomRewindContext
  itemKey: string
  turn: MessageTurn | undefined
  /** Lazily: only an opened dialog needs the message text. */
  getText: () => string
  busy: boolean
}) {
  const t = useTranslations("Phantom.rewind")
  const [open, setOpen] = useState(false)
  const [text, setText] = useState("")

  return (
    <>
      <MessageAction
        tooltip={t("action")}
        className="opacity-0 group-hover/user-msg:opacity-100 transition-opacity self-end"
        onClick={() => {
          setText(getText())
          setOpen(true)
        }}
        size="icon-xs"
      >
        <History size={12} />
      </MessageAction>
      {open ? (
        <RewindDialog
          open={open}
          onOpenChange={setOpen}
          context={context}
          itemKey={itemKey}
          turn={turn}
          text={text}
          busy={busy}
        />
      ) : null}
    </>
  )
})
