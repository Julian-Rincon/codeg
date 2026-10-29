"use client"

/**
 * The composer's half of Phantom's memory editor, for Claude Code sessions:
 *
 * - `#` quick-add (Claude Code's convention): when the WHOLE draft is one line
 *   starting with `#`, submitting it does not send a message — a small chooser
 *   asks where to keep the note (project or user CLAUDE.md) and appends
 *   `- <note>` there. The composer only asks after its own Enter handling ran,
 *   so IME composition and the `/` and `@` menus are untouched; it can be
 *   switched off in Settings.
 * - the "Memory (CLAUDE.md)" entry of the "+" menu, which opens the editor.
 *
 * One hook so the composer's edit stays a few lines: call it, route the draft
 * through `interceptDraft`, render `element`.
 */

import { useCallback, useEffect, useRef, useState } from "react"
import dynamic from "next/dynamic"
import { useTranslations } from "next-intl"
import { BookMarked, Loader2 } from "lucide-react"
import { toast } from "sonner"

import { Button } from "@/components/ui/button"
import { toErrorMessage } from "@/lib/app-error"
import {
  QUICK_ADD_SCOPES,
  type QuickAddScope,
  appendMemoryNote,
  formatMemoryBullet,
  parseMemoryQuickAdd,
  supportsClaudeMemory,
  useMemoryQuickAddEnabled,
} from "@/lib/phantom-memory"
import type { AgentType, PromptDraft } from "@/lib/types"

// The editor pulls in Monaco; load it only when someone opens it.
const MemoryEditorDialog = dynamic(
  async () =>
    (await import("@/components/phantom/memory-editor-dialog"))
      .MemoryEditorDialog,
  { ssr: false }
)

const SCOPE_KEYS: Record<QuickAddScope, string> = { project: "p", user: "u" }

export interface MemoryQuickAddChooserProps {
  note: string
  onPick: (scope: QuickAddScope) => void
  onCancel: () => void
  busy: boolean
}

export function MemoryQuickAddChooser({
  note,
  onPick,
  onCancel,
  busy,
}: MemoryQuickAddChooserProps) {
  const t = useTranslations("Phantom.memory")
  const firstRef = useRef<HTMLButtonElement>(null)

  useEffect(() => {
    firstRef.current?.focus()
  }, [])

  return (
    <div
      role="dialog"
      aria-label={t("quickAdd.title")}
      className="absolute bottom-full left-0 z-50 mb-1 w-[min(26rem,100%)] rounded-xl border border-border bg-popover p-3 text-xs shadow-lg"
      onKeyDown={(e) => {
        if (e.nativeEvent.isComposing) return
        if (e.key === "Escape") {
          e.preventDefault()
          e.stopPropagation()
          onCancel()
          return
        }
        if (e.metaKey || e.ctrlKey || e.altKey || busy) return
        const scope = QUICK_ADD_SCOPES.find(
          (s) => SCOPE_KEYS[s] === e.key.toLowerCase()
        )
        if (scope) {
          e.preventDefault()
          e.stopPropagation()
          onPick(scope)
        }
      }}
    >
      <div className="mb-1 flex items-center gap-1.5 font-medium">
        <BookMarked className="size-3.5 text-muted-foreground" />
        {t("quickAdd.title")}
      </div>
      <p className="mb-2 truncate font-mono text-muted-foreground" title={note}>
        {formatMemoryBullet(note)}
      </p>
      <div className="flex flex-wrap items-center gap-1.5">
        {QUICK_ADD_SCOPES.map((scope, i) => (
          <Button
            key={scope}
            ref={i === 0 ? firstRef : undefined}
            type="button"
            size="sm"
            variant={i === 0 ? "default" : "outline"}
            disabled={busy}
            onClick={() => onPick(scope)}
            title={t(`scopes.${scope}.path`)}
          >
            {t(`scopes.${scope}.label`)}
            <kbd className="ml-1 text-[10px] opacity-60">
              {SCOPE_KEYS[scope].toUpperCase()}
            </kbd>
          </Button>
        ))}
        <Button
          type="button"
          size="sm"
          variant="ghost"
          disabled={busy}
          onClick={onCancel}
        >
          {t("quickAdd.cancel")}
        </Button>
        {busy ? (
          <Loader2 className="size-3.5 animate-spin text-muted-foreground" />
        ) : null}
      </div>
      <p className="mt-2 text-[11px] leading-4 text-muted-foreground">
        {t("quickAdd.hint")}
      </p>
    </div>
  )
}

export interface PhantomMemoryComposerOptions {
  agentType: AgentType | null | undefined
  folderPath: string | null | undefined
  /** The note landed: clear the composer as a send would. */
  onNoteSaved: () => void
  /** The chooser closed without saving: give the editor its focus back. */
  onDismiss: () => void
}

export function usePhantomMemoryComposer({
  agentType,
  folderPath,
  onNoteSaved,
  onDismiss,
}: PhantomMemoryComposerOptions) {
  const t = useTranslations("Phantom.memory")
  const quickAddEnabled = useMemoryQuickAddEnabled()
  const supported = supportsClaudeMemory(agentType) && !!folderPath
  const [pendingNote, setPendingNote] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [editorOpen, setEditorOpen] = useState(false)

  /** `true` when the draft was a quick-add and must NOT be sent. */
  const interceptDraft = useCallback(
    (draft: PromptDraft | null): boolean => {
      if (!supported || !quickAddEnabled) return false
      const note = parseMemoryQuickAdd(draft)
      if (note == null) return false
      setPendingNote(note)
      return true
    },
    [supported, quickAddEnabled]
  )

  const cancel = useCallback(() => {
    setPendingNote(null)
    onDismiss()
  }, [onDismiss])

  const pick = useCallback(
    async (scope: QuickAddScope) => {
      if (!folderPath || pendingNote == null || busy) return
      setBusy(true)
      try {
        await appendMemoryNote(folderPath, scope, pendingNote)
        toast.success(t("quickAdd.saved", { file: t(`scopes.${scope}.path`) }))
        setPendingNote(null)
        onNoteSaved()
      } catch (err) {
        toast.error(t("quickAdd.failed", { message: toErrorMessage(err) }))
      } finally {
        setBusy(false)
      }
    },
    [folderPath, pendingNote, busy, onNoteSaved, t]
  )

  const openEditor = useCallback(() => setEditorOpen(true), [])

  const element = supported ? (
    <>
      {pendingNote != null ? (
        <MemoryQuickAddChooser
          note={pendingNote}
          busy={busy}
          onPick={(scope) => void pick(scope)}
          onCancel={cancel}
        />
      ) : null}
      {editorOpen && folderPath ? (
        <MemoryEditorDialog
          open={editorOpen}
          onOpenChange={setEditorOpen}
          folderPath={folderPath}
        />
      ) : null}
    </>
  ) : null

  return {
    interceptDraft,
    openEditor: supported ? openEditor : undefined,
    element,
  }
}
