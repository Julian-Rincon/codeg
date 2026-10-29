/**
 * Claude Code's `/context` report, read back from the chat.
 *
 * Why the chat: claude-agent-acp (0.94 – 0.98) lists `/context` in its
 * `LOCAL_ONLY_COMMANDS` — Claude Code answers it locally, without a model
 * call — and forwards the answer only as markdown text (an
 * `agent_message_chunk`). The SDK's structured `context_usage` twin and its
 * `getContextUsage` control request never reach an ACP client. So Phantom sends
 * `/context` on an explicit click and parses the markdown it gets back:
 *
 * ```
 * ## Context Usage
 * **Model:** claude-…
 * **Tokens:** 15.2k / 200k (8%)
 * ### Estimated usage by category
 * | Category | Tokens | Percentage |
 * |----------|--------|------------|
 * | System prompt | 2.9k | 1.5% |
 * | Free space | 140k | 70.1% |
 * | Autocompact buffer | 45.0k | 22.5% |
 * ### Memory Files
 * | Type | Path | Tokens |
 * ```
 */

import type { ConversationRuntimeSession } from "@/stores/conversation-runtime-store"
import type { ContentBlock, MessageTurn } from "@/lib/types"

export const CONTEXT_REPORT_HEADING = "## Context Usage"

export type ContextCategoryKind = "used" | "free" | "buffer" | "deferred"

export interface ContextCategory {
  name: string
  tokens: number
  percent: number | null
  kind: ContextCategoryKind
}

export interface ContextMemoryFile {
  type: string
  path: string
  tokens: number
}

export interface ContextBreakdown {
  model: string | null
  usedTokens: number | null
  maxTokens: number | null
  percent: number | null
  categories: ContextCategory[]
  memoryFiles: ContextMemoryFile[]
}

/** `868`, `2.9k`, `1,234`, `1.2M` → tokens. `null` when not a count. */
export function parseTokenCount(raw: string): number | null {
  const m = /^\s*([\d.,]+)\s*([kKmM])?\s*$/.exec(raw)
  if (!m) return null
  const n = Number(m[1].replace(/,/g, ""))
  if (!Number.isFinite(n)) return null
  const mult = m[2] ? (m[2].toLowerCase() === "k" ? 1e3 : 1e6) : 1
  return Math.round(n * mult)
}

function parsePercent(raw: string): number | null {
  const m = /^\s*([\d.]+)\s*%\s*$/.exec(raw)
  return m ? Number(m[1]) : null
}

function classify(name: string): ContextCategoryKind {
  const lower = name.toLowerCase()
  if (lower === "free space") return "free"
  if (lower.includes("autocompact buffer")) return "buffer"
  if (lower.includes("deferred")) return "deferred"
  return "used"
}

/** The rows of the first markdown table after `heading`, separator dropped. */
function tableAfter(lines: string[], heading: RegExp): string[][] {
  const start = lines.findIndex((l) => heading.test(l.trim()))
  if (start < 0) return []
  const rows: string[][] = []
  let inTable = false
  for (let i = start + 1; i < lines.length; i++) {
    const line = lines[i].trim()
    if (line.startsWith("#")) break
    if (!line.startsWith("|")) {
      if (inTable) break
      continue
    }
    inTable = true
    const cells = line
      .replace(/^\|/, "")
      .replace(/\|$/, "")
      .split("|")
      .map((c) => c.trim())
    if (cells.every((c) => /^:?-+:?$/.test(c))) continue
    rows.push(cells)
  }
  // First row is the header.
  return rows.slice(1)
}

/** Parse a `/context` report; `null` when `markdown` isn't one. */
export function parseContextReport(markdown: string): ContextBreakdown | null {
  const at = markdown.indexOf(CONTEXT_REPORT_HEADING)
  if (at < 0) return null
  const lines = markdown.slice(at).split(/\r?\n/)

  let model: string | null = null
  let usedTokens: number | null = null
  let maxTokens: number | null = null
  let percent: number | null = null
  for (const line of lines) {
    const modelMatch = /\*\*Model:\*\*\s*(.+?)\s*$/.exec(line)
    if (modelMatch && model == null) model = modelMatch[1]
    const tokMatch =
      /\*\*Tokens:\*\*\s*([\d.,]+\s*[kKmM]?)\s*\/\s*([\d.,]+\s*[kKmM]?)(?:\s*\(\s*([\d.]+)\s*%\s*\))?/.exec(
        line
      )
    if (tokMatch && usedTokens == null) {
      usedTokens = parseTokenCount(tokMatch[1])
      maxTokens = parseTokenCount(tokMatch[2])
      percent = tokMatch[3] != null ? Number(tokMatch[3]) : null
    }
  }

  const categories: ContextCategory[] = []
  for (const cells of tableAfter(lines, /^###\s+.*categor/i)) {
    if (cells.length < 2) continue
    const tokens = parseTokenCount(cells[1])
    if (tokens == null) continue
    const name = cells[0].replace(/\*\*/g, "")
    categories.push({
      name,
      tokens,
      percent: cells[2] != null ? parsePercent(cells[2]) : null,
      kind: classify(name),
    })
  }

  const memoryFiles: ContextMemoryFile[] = []
  for (const cells of tableAfter(lines, /^###\s+memory files/i)) {
    if (cells.length < 3) continue
    const tokens = parseTokenCount(cells[2])
    if (tokens == null) continue
    memoryFiles.push({ type: cells[0], path: cells[1], tokens })
  }

  if (categories.length === 0 && usedTokens == null) return null
  return { model, usedTokens, maxTokens, percent, categories, memoryFiles }
}

/**
 * Autocompact fires once usage reaches the window minus the buffer Claude Code
 * reserves for it. Only known from a report (the adapter doesn't publish it).
 */
export function autocompactThreshold(
  breakdown: ContextBreakdown | null,
  windowSize: number | null
): number | null {
  const buffer = breakdown?.categories.find((c) => c.kind === "buffer")
  const max = breakdown?.maxTokens ?? windowSize
  if (!buffer || max == null || max <= 0) return null
  return Math.max(0, max - buffer.tokens)
}

function blocksText(blocks: ContentBlock[] | { type: string }[]): string {
  let text = ""
  for (const block of blocks) {
    if (block.type === "text") text += (block as { text: string }).text
  }
  return text
}

export interface ContextReportHit {
  markdown: string
  /** ISO timestamp of the turn that carried it; `null` while still streaming. */
  at: string | null
}

function latestInTurns(
  turns: MessageTurn[] | undefined
): ContextReportHit | null {
  if (!turns) return null
  for (let i = turns.length - 1; i >= 0; i--) {
    const turn = turns[i]
    if (turn.role !== "assistant") continue
    const text = blocksText(turn.blocks)
    if (text.includes(CONTEXT_REPORT_HEADING)) {
      return { markdown: text, at: turn.timestamp ?? null }
    }
  }
  return null
}

/**
 * The newest `/context` report in a conversation: the reply still streaming,
 * then this session's completed turns, then the persisted history.
 */
export function findLatestContextReport(
  session: ConversationRuntimeSession | null | undefined
): ContextReportHit | null {
  if (!session) return null
  const live = session.liveMessage
  if (live && live.role === "assistant") {
    const text = blocksText(live.content)
    if (text.includes(CONTEXT_REPORT_HEADING))
      return { markdown: text, at: null }
  }
  return (
    latestInTurns(session.localTurns) ?? latestInTurns(session.detail?.turns)
  )
}
