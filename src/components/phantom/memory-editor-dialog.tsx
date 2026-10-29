"use client"

/**
 * Phantom's `/memory`: the Claude Code memory files that apply to a workspace
 * folder, listed with which exist, editable in the app's Monaco editor.
 *
 * Each file keeps its own buffer while the dialog is open, so switching files
 * never drops edits. Saving sends the mtime the buffer was loaded at; if the
 * file changed on disk meanwhile (Claude Code itself appends to these files),
 * nothing is written and the user chooses: reload theirs, or overwrite.
 */

import { useCallback, useEffect, useRef, useState } from "react"
import dynamic from "next/dynamic"
import { useTranslations } from "next-intl"
import { AlertTriangle, FilePlus2, FileText, Loader2 } from "lucide-react"
import { toast } from "sonner"

import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog"
import { Button } from "@/components/ui/button"
import { Switch } from "@/components/ui/switch"
import { cn } from "@/lib/utils"
import { toErrorMessage } from "@/lib/app-error"
import { defineMonacoThemes, useMonacoThemeSync } from "@/lib/monaco-themes"
import { useEditorFont } from "@/hooks/use-appearance"
import {
  type MemoryFileInfo,
  type MemoryScope,
  listMemoryFiles,
  readMemoryFile,
  setMemoryQuickAddEnabled,
  useMemoryQuickAddEnabled,
  writeMemoryFile,
} from "@/lib/phantom-memory"
import "@/lib/monaco-local"

const MonacoEditor = dynamic(async () => import("@monaco-editor/react"), {
  ssr: false,
})

interface Buffer {
  loaded: string
  draft: string
  /** mtime the buffer was read at; `null` = the file did not exist. */
  mtime: number | null
  exists: boolean
}

type Conflict = { scope: MemoryScope; currentMtime: number | null }

export function MemoryEditorDialog({
  open,
  onOpenChange,
  folderPath,
  initialScope = "project",
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  folderPath: string
  initialScope?: MemoryScope
}) {
  const t = useTranslations("Phantom.memory")
  const { editorFontStack, editorFontSize } = useEditorFont()
  const monacoTheme = useMonacoThemeSync()
  const quickAddEnabled = useMemoryQuickAddEnabled()

  const [files, setFiles] = useState<MemoryFileInfo[] | null>(null)
  const [listError, setListError] = useState<string | null>(null)
  const [scope, setScope] = useState<MemoryScope>(initialScope)
  const [buffers, setBuffers] = useState<Partial<Record<MemoryScope, Buffer>>>(
    {}
  )
  const [loadingScope, setLoadingScope] = useState<MemoryScope | null>(null)
  // Per-scope read failures. Without them a failed read left the pane on its
  // spinner for good, with the reason buried in a toast that had already faded.
  const [loadErrors, setLoadErrors] = useState<
    Partial<Record<MemoryScope, string>>
  >({})
  const [saving, setSaving] = useState(false)
  const [conflict, setConflict] = useState<Conflict | null>(null)
  const [confirmClose, setConfirmClose] = useState(false)

  const refreshList = useCallback(async () => {
    try {
      setFiles(await listMemoryFiles(folderPath))
      setListError(null)
    } catch (err) {
      setListError(toErrorMessage(err))
    }
  }, [folderPath])

  const load = useCallback(
    async (target: MemoryScope) => {
      setLoadingScope(target)
      setLoadErrors((prev) => {
        if (prev[target] == null) return prev
        const next = { ...prev }
        delete next[target]
        return next
      })
      try {
        const file = await readMemoryFile(folderPath, target)
        setBuffers((prev) => ({
          ...prev,
          [target]: {
            loaded: file.content,
            draft: file.content,
            mtime: file.mtime_ms ?? null,
            exists: file.exists,
          },
        }))
      } catch (err) {
        const message = toErrorMessage(err)
        setLoadErrors((prev) => ({ ...prev, [target]: message }))
        toast.error(t("editor.loadFailed", { message }))
      } finally {
        setLoadingScope((cur) => (cur === target ? null : cur))
      }
    },
    [folderPath, t]
  )

  useEffect(() => {
    if (!open) return
    void refreshList()
  }, [open, refreshList])

  const current = buffers[scope]
  const info = files?.find((f) => f.scope === scope) ?? null
  useEffect(() => {
    if (!open || current || info?.error) return
    void load(scope)
  }, [open, scope, current, info?.error, load])

  const isDirty = (b: Buffer | undefined) => !!b && b.draft !== b.loaded
  const anyDirty = Object.values(buffers).some(isDirty)

  const save = useCallback(
    async (target: MemoryScope, expected: number | null) => {
      const buf = buffers[target]
      if (!buf) return
      setSaving(true)
      try {
        const result = await writeMemoryFile(
          folderPath,
          target,
          buf.draft,
          expected
        )
        if (result.status === "conflict") {
          setConflict({
            scope: target,
            currentMtime: result.current_mtime_ms ?? null,
          })
          return
        }
        setConflict(null)
        setBuffers((prev) => ({
          ...prev,
          [target]: {
            loaded: buf.draft,
            draft: buf.draft,
            mtime: result.mtime_ms,
            exists: true,
          },
        }))
        toast.success(t("editor.saved"))
        void refreshList()
      } catch (err) {
        toast.error(t("editor.saveFailed", { message: toErrorMessage(err) }))
      } finally {
        setSaving(false)
      }
    },
    [buffers, folderPath, refreshList, t]
  )

  // Monaco's Ctrl/Cmd+S binding is registered once; it calls the latest save.
  const saveCurrentRef = useRef<() => void>(() => {})
  useEffect(() => {
    saveCurrentRef.current = () => {
      const buf = buffers[scope]
      if (buf && isDirty(buf) && !saving) void save(scope, buf.mtime)
    }
  }, [buffers, scope, save, saving])

  const handleOpenChange = (next: boolean) => {
    if (!next && anyDirty && !confirmClose) {
      setConfirmClose(true)
      return
    }
    setConfirmClose(false)
    onOpenChange(next)
  }

  const scopeLabel = (s: MemoryScope) => t(`scopes.${s}.label`)

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="flex max-h-[85vh] flex-col sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>{t("editor.title")}</DialogTitle>
          <DialogDescription>{t("editor.description")}</DialogDescription>
        </DialogHeader>

        <div className="flex min-h-0 flex-1 gap-3">
          <ul
            className="flex w-56 shrink-0 flex-col gap-1"
            aria-label={t("editor.files")}
          >
            {listError ? (
              <li className="text-xs text-destructive">{listError}</li>
            ) : null}
            {(files ?? []).map((f) => (
              <li key={f.scope}>
                <button
                  type="button"
                  onClick={() => {
                    setScope(f.scope)
                    setConflict(null)
                  }}
                  aria-current={f.scope === scope ? "true" : undefined}
                  className={cn(
                    "w-full rounded-md border px-2 py-1.5 text-left text-xs transition-colors",
                    f.scope === scope
                      ? "border-primary/60 bg-accent"
                      : "border-transparent hover:bg-accent/60"
                  )}
                >
                  <span className="flex items-center gap-1.5 font-medium">
                    {f.exists ? (
                      <FileText className="size-3.5 shrink-0" />
                    ) : (
                      <FilePlus2 className="size-3.5 shrink-0 text-muted-foreground" />
                    )}
                    <span className="truncate">{scopeLabel(f.scope)}</span>
                    {isDirty(buffers[f.scope]) ? (
                      <span
                        className="ml-auto size-1.5 shrink-0 rounded-full bg-primary"
                        aria-label={t("editor.unsaved")}
                      />
                    ) : null}
                  </span>
                  <span
                    className="block truncate font-mono text-[10px] text-muted-foreground"
                    title={f.path}
                  >
                    {f.path}
                  </span>
                  <span
                    className={cn(
                      "block text-[10px]",
                      f.error ? "text-destructive" : "text-muted-foreground"
                    )}
                  >
                    {f.error
                      ? t("editor.blocked")
                      : f.exists
                        ? t("editor.exists")
                        : t("editor.missing")}
                  </span>
                </button>
              </li>
            ))}
          </ul>

          <div className="flex min-w-0 flex-1 flex-col gap-2">
            {info?.error ? (
              <p className="flex items-center gap-1.5 text-xs text-destructive">
                <AlertTriangle className="size-3.5 shrink-0" />
                {t("editor.blockedDetail", { reason: info.error })}
              </p>
            ) : loadErrors[scope] ? (
              <div className="flex h-[55vh] flex-col items-center justify-center gap-2 px-4 text-center">
                <AlertTriangle className="size-4 shrink-0 text-destructive" />
                <p className="text-xs text-destructive">
                  {t("editor.loadFailed", { message: loadErrors[scope] })}
                </p>
                <Button
                  type="button"
                  size="sm"
                  variant="outline"
                  disabled={loadingScope === scope}
                  onClick={() => void load(scope)}
                >
                  {t("editor.retry")}
                </Button>
              </div>
            ) : !current || loadingScope === scope ? (
              <div className="flex h-[55vh] items-center justify-center">
                <Loader2 className="size-4 animate-spin text-muted-foreground" />
              </div>
            ) : (
              <>
                {!current.exists ? (
                  <p className="text-xs text-muted-foreground">
                    {t("editor.willCreate")}
                  </p>
                ) : null}
                <div className="min-h-0 flex-1 overflow-hidden rounded-md border">
                  <MonacoEditor
                    key={scope}
                    height="55vh"
                    language="markdown"
                    theme={monacoTheme}
                    beforeMount={defineMonacoThemes}
                    value={current.draft}
                    onChange={(value) =>
                      setBuffers((prev) => {
                        const buf = prev[scope]
                        if (!buf) return prev
                        return {
                          ...prev,
                          [scope]: { ...buf, draft: value ?? "" },
                        }
                      })
                    }
                    onMount={(editor, monaco) => {
                      editor.addCommand(
                        monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyS,
                        () => saveCurrentRef.current()
                      )
                    }}
                    loading={
                      <div className="flex h-full items-center justify-center">
                        <Loader2 className="size-4 animate-spin text-muted-foreground" />
                      </div>
                    }
                    options={{
                      fontFamily: editorFontStack,
                      fontSize: editorFontSize,
                      minimap: { enabled: false },
                      wordWrap: "on",
                      scrollBeyondLastLine: false,
                      lineNumbers: "on",
                      tabSize: 2,
                      automaticLayout: true,
                    }}
                  />
                </div>
              </>
            )}

            {conflict && conflict.scope === scope ? (
              <div
                role="alert"
                className="flex flex-wrap items-center gap-2 rounded-md border border-destructive/40 bg-destructive/5 px-2 py-1.5 text-xs"
              >
                <AlertTriangle className="size-3.5 shrink-0 text-destructive" />
                <span className="min-w-0 flex-1">
                  {conflict.currentMtime == null
                    ? t("editor.conflictDeleted")
                    : t("editor.conflict")}
                </span>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => {
                    setConflict(null)
                    void load(scope)
                  }}
                >
                  {t("editor.reload")}
                </Button>
                <Button
                  size="sm"
                  variant="destructive"
                  disabled={saving}
                  onClick={() => void save(scope, conflict.currentMtime)}
                >
                  {t("editor.overwrite")}
                </Button>
              </div>
            ) : null}
          </div>
        </div>

        <DialogFooter className="items-center gap-2 sm:justify-between">
          <label className="flex items-center gap-2 text-xs text-muted-foreground">
            <Switch
              checked={quickAddEnabled}
              onCheckedChange={setMemoryQuickAddEnabled}
            />
            {t("settings.title")}
          </label>
          <div className="flex items-center gap-2">
            {confirmClose ? (
              <>
                <span className="text-xs text-muted-foreground">
                  {t("editor.discardPrompt")}
                </span>
                <Button
                  variant="outline"
                  onClick={() => setConfirmClose(false)}
                >
                  {t("editor.keepEditing")}
                </Button>
                <Button
                  variant="destructive"
                  onClick={() => {
                    setConfirmClose(false)
                    onOpenChange(false)
                  }}
                >
                  {t("editor.discard")}
                </Button>
              </>
            ) : (
              <>
                <Button
                  variant="outline"
                  onClick={() => handleOpenChange(false)}
                >
                  {t("editor.close")}
                </Button>
                <Button
                  disabled={
                    !current ||
                    (current.exists && !isDirty(current)) ||
                    saving ||
                    !!info?.error
                  }
                  onClick={() => current && void save(scope, current.mtime)}
                >
                  {saving ? (
                    <Loader2 className="size-3.5 animate-spin" />
                  ) : null}
                  {current && !current.exists
                    ? t("editor.create")
                    : t("editor.save")}
                </Button>
              </>
            )}
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
