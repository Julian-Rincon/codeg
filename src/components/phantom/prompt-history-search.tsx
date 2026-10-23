"use client"

import { useEffect, useMemo, useRef, useState } from "react"
import { useTranslations } from "next-intl"
import { History } from "lucide-react"
import {
  loadRecallHistory,
  promptSearchKeyAction,
  searchPromptHistory,
} from "@/lib/phantom-prompt-history"
import { cn } from "@/lib/utils"

interface PromptHistorySearchProps {
  /** Working directory of the composer; its prompts rank first. */
  folder: string | null
  /** The session's own prompts, oldest first (upstream `getSentHistory`). */
  getSessionHistory?: () => string[]
  onPick: (text: string) => void
  onClose: () => void
}

/**
 * Ctrl+R prompt-history search, hung above the composer the way the `/` menu
 * is. Focus moves into its search box; Enter inserts the selected prompt,
 * Escape (or blur) closes and hands focus back to the editor via `onClose`.
 */
export function PromptHistorySearch({
  folder,
  getSessionHistory,
  onPick,
  onClose,
}: PromptHistorySearchProps) {
  const t = useTranslations("Phantom.promptHistory")
  const [query, setQuery] = useState("")
  const [selected, setSelected] = useState(0)
  const listRef = useRef<HTMLDivElement>(null)
  // Read once per opening: the list cannot change while the box has focus.
  const [ranked] = useState(() =>
    loadRecallHistory(getSessionHistory?.() ?? [], folder).reverse()
  )
  const matches = useMemo(
    () => searchPromptHistory(ranked, query),
    [ranked, query]
  )

  useEffect(() => {
    listRef.current
      ?.querySelector<HTMLElement>(`[data-index="${selected}"]`)
      ?.scrollIntoView({ block: "nearest" })
  }, [selected])

  return (
    <div
      data-testid="prompt-history-search"
      className="absolute bottom-full left-0 right-0 z-50 mb-1 flex max-h-[min(20rem,50dvh)] flex-col overflow-hidden rounded-xl border border-border bg-popover shadow-lg"
    >
      <div className="flex items-center gap-2 border-b border-border px-3 py-2">
        <History className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
        <input
          autoFocus
          value={query}
          placeholder={t("placeholder")}
          aria-label={t("title")}
          role="combobox"
          aria-expanded="true"
          aria-controls="phantom-prompt-history-list"
          aria-activedescendant={
            matches.length > 0
              ? `phantom-prompt-history-${selected}`
              : undefined
          }
          className="min-w-0 flex-1 bg-transparent text-sm outline-none placeholder:text-muted-foreground"
          onChange={(e) => {
            setQuery(e.target.value)
            setSelected(0)
          }}
          onBlur={onClose}
          onKeyDown={(e) => {
            const action = promptSearchKeyAction(
              e.nativeEvent,
              selected,
              matches.length
            )
            if (!action) return
            e.preventDefault()
            e.stopPropagation()
            if (action.type === "close") onClose()
            else if (action.type === "move") setSelected(action.index)
            else onPick(matches[selected])
          }}
        />
        <span className="shrink-0 text-[0.6875rem] text-muted-foreground">
          {t("hint")}
        </span>
      </div>
      <div
        ref={listRef}
        id="phantom-prompt-history-list"
        role="listbox"
        className="flex-1 overflow-y-auto p-1"
      >
        {matches.length === 0 ? (
          <div className="px-2 py-1.5 text-sm text-muted-foreground">
            {ranked.length === 0 ? t("empty") : t("noMatches")}
          </div>
        ) : (
          matches.map((text, i) => (
            <div
              key={text}
              id={`phantom-prompt-history-${i}`}
              data-index={i}
              role="option"
              aria-selected={i === selected}
              title={text}
              // mousedown, not click: the input's blur would close us first.
              onMouseDown={(e) => {
                e.preventDefault()
                onPick(text)
              }}
              onMouseEnter={() => setSelected(i)}
              className={cn(
                "cursor-pointer truncate rounded-md px-2 py-1.5 text-sm",
                i === selected && "bg-accent text-accent-foreground"
              )}
            >
              {text.replace(/\s+/g, " ")}
            </div>
          ))
        )}
      </div>
    </div>
  )
}
