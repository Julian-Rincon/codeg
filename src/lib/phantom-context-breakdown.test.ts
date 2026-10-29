import { describe, expect, it } from "vitest"
import {
  autocompactThreshold,
  findLatestContextReport,
  parseContextReport,
  parseTokenCount,
} from "./phantom-context-breakdown"
import type { ConversationRuntimeSession } from "@/stores/conversation-runtime-store"
import type { MessageTurn } from "@/lib/types"

// Shape of Claude Code's SDK-mode `/context` markdown (template strings
// verified in the bundled CLI binary: "## Context Usage", "**Model:** ",
// "**Tokens:** ", "### Estimated usage by category", "| Category | Tokens |
// Percentage |", "| Free space | ", "| Autocompact buffer | ", "### Memory
// Files", "| Type | Path | Tokens |").
const SAMPLE_REPORT = `## Context Usage

**Model:** claude-sonnet-4-5
**Tokens:** 15.2k / 200k (8%)

### Estimated usage by category

| Category | Tokens | Percentage |
|----------|--------|------------|
| System prompt | 2.9k | 1.5% |
| System tools | 11.4k | 5.7% |
| MCP tools | 1.3k | 0.7% |
| Memory files | 868 | 0.4% |
| Messages | 8 | 0.0% |
| Free space | 138.5k | 69.2% |
| Autocompact buffer | 45.0k | 22.5% |

### MCP Tools

| Tool | Server | Tokens |
|------|--------|--------|
| mcp__codeg__delegate | codeg | 1.3k |

### Memory Files

| Type | Path | Tokens |
|------|------|--------|
| User | /home/u/.claude/CLAUDE.md | 500 |
| Project | /w/p/CLAUDE.md | 368 |
`

describe("parseTokenCount", () => {
  it("reads plain, k, M and comma forms", () => {
    expect(parseTokenCount("868")).toBe(868)
    expect(parseTokenCount("2.9k")).toBe(2900)
    expect(parseTokenCount("1.2M")).toBe(1_200_000)
    expect(parseTokenCount("1,234")).toBe(1234)
    expect(parseTokenCount("n/a")).toBeNull()
  })
})

describe("parseContextReport", () => {
  it("parses the header, categories and memory files", () => {
    const r = parseContextReport(SAMPLE_REPORT)!
    expect(r.model).toBe("claude-sonnet-4-5")
    expect(r.usedTokens).toBe(15_200)
    expect(r.maxTokens).toBe(200_000)
    expect(r.percent).toBe(8)
    expect(r.categories.map((c) => [c.name, c.tokens, c.kind])).toEqual([
      ["System prompt", 2900, "used"],
      ["System tools", 11_400, "used"],
      ["MCP tools", 1300, "used"],
      ["Memory files", 868, "used"],
      ["Messages", 8, "used"],
      ["Free space", 138_500, "free"],
      ["Autocompact buffer", 45_000, "buffer"],
    ])
    expect(r.categories[0].percent).toBe(1.5)
    // The MCP tools table is not mistaken for memory files.
    expect(r.memoryFiles).toEqual([
      { type: "User", path: "/home/u/.claude/CLAUDE.md", tokens: 500 },
      { type: "Project", path: "/w/p/CLAUDE.md", tokens: 368 },
    ])
  })

  it("finds the report inside a longer message", () => {
    expect(parseContextReport(`intro\n\n${SAMPLE_REPORT}`)?.usedTokens).toBe(
      15_200
    )
  })

  it("is null for anything that is not a report", () => {
    expect(parseContextReport("hello")).toBeNull()
    expect(parseContextReport("## Context Usage\n\nnothing here")).toBeNull()
  })

  it("derives the autocompact threshold from the buffer row", () => {
    const r = parseContextReport(SAMPLE_REPORT)
    expect(autocompactThreshold(r, null)).toBe(155_000)
    expect(autocompactThreshold(null, 200_000)).toBeNull()
  })
})

function turn(
  role: MessageTurn["role"],
  text: string,
  ts = "2026-10-28T00:00:00Z"
): MessageTurn {
  return { id: ts, role, blocks: [{ type: "text", text }], timestamp: ts }
}

function session(
  parts: Partial<ConversationRuntimeSession>
): ConversationRuntimeSession {
  return {
    liveMessage: null,
    localTurns: [],
    detail: null,
    ...parts,
  } as ConversationRuntimeSession
}

describe("findLatestContextReport", () => {
  it("prefers the newest assistant report, ignoring user turns", () => {
    const hit = findLatestContextReport(
      session({
        localTurns: [
          turn("assistant", SAMPLE_REPORT.replace("15.2k", "1k"), "t1"),
          turn("user", "/context", "t2"),
          turn("assistant", SAMPLE_REPORT, "t3"),
          turn("user", `pasted ${SAMPLE_REPORT}`, "t4"),
        ],
      })
    )
    expect(hit?.at).toBe("t3")
    expect(parseContextReport(hit!.markdown)?.usedTokens).toBe(15_200)
  })

  it("reads the streaming reply first and falls back to history", () => {
    const live = findLatestContextReport(
      session({
        liveMessage: {
          id: "l",
          role: "assistant",
          content: [{ type: "text", text: SAMPLE_REPORT }],
          startedAt: 0,
        },
      })
    )
    expect(live?.at).toBeNull()

    const persisted = findLatestContextReport(
      session({
        detail: {
          turns: [turn("assistant", SAMPLE_REPORT, "old")],
        } as ConversationRuntimeSession["detail"],
      })
    )
    expect(persisted?.at).toBe("old")
    expect(findLatestContextReport(session({}))).toBeNull()
    expect(findLatestContextReport(null)).toBeNull()
  })
})
