import { beforeEach, describe, expect, it } from "vitest"
import {
  formatMemoryBullet,
  getMemoryQuickAddEnabled,
  parseMemoryQuickAdd,
  setMemoryQuickAddEnabled,
  supportsClaudeMemory,
} from "./phantom-memory"
import type { PromptDraft } from "@/lib/types"

const text = (t: string): PromptDraft => ({
  blocks: [{ type: "text", text: t }],
  displayText: t,
})

describe("parseMemoryQuickAdd", () => {
  it("takes a whole single-line draft starting with #", () => {
    expect(parseMemoryQuickAdd(text("# use pnpm, not npm"))).toBe(
      "use pnpm, not npm"
    )
    expect(parseMemoryQuickAdd(text("#no space"))).toBe("no space")
    expect(parseMemoryQuickAdd(text("  # padded  "))).toBe("padded")
  })

  it("strips exactly one #, as Claude Code does", () => {
    expect(parseMemoryQuickAdd(text("## heading-ish"))).toBe("# heading-ish")
  })

  it("leaves ordinary messages alone", () => {
    expect(parseMemoryQuickAdd(text("fix #123 please"))).toBeNull()
    expect(parseMemoryQuickAdd(text("/compact"))).toBeNull()
    expect(parseMemoryQuickAdd(text("@file.ts # look"))).toBeNull()
    expect(parseMemoryQuickAdd(text(""))).toBeNull()
  })

  it("needs something after the #", () => {
    expect(parseMemoryQuickAdd(text("#"))).toBeNull()
    expect(parseMemoryQuickAdd(text("#   "))).toBeNull()
  })

  it("rejects multi-line drafts (a markdown message, not a note)", () => {
    expect(parseMemoryQuickAdd(text("# Title\nbody"))).toBeNull()
    expect(parseMemoryQuickAdd(text("# Title\r\nbody"))).toBeNull()
  })

  it("rejects drafts carrying anything but one text block", () => {
    expect(parseMemoryQuickAdd(null)).toBeNull()
    expect(
      parseMemoryQuickAdd({
        blocks: [
          { type: "text", text: "# note" },
          { type: "text", text: "more" },
        ],
        displayText: "# note more",
      })
    ).toBeNull()
    expect(
      parseMemoryQuickAdd({
        blocks: [{ type: "image", data: "x", mime_type: "image/png" }],
        displayText: "# note",
      })
    ).toBeNull()
  })

  it("formats the appended line", () => {
    expect(formatMemoryBullet("use pnpm")).toBe("- use pnpm")
  })
})

describe("quick-add preference", () => {
  beforeEach(() => localStorage.clear())

  it("defaults on and round-trips off/on", () => {
    expect(getMemoryQuickAddEnabled()).toBe(true)
    setMemoryQuickAddEnabled(false)
    expect(getMemoryQuickAddEnabled()).toBe(false)
    setMemoryQuickAddEnabled(true)
    expect(getMemoryQuickAddEnabled()).toBe(true)
  })

  it("is offered for Claude Code only", () => {
    expect(supportsClaudeMemory("claude_code")).toBe(true)
    expect(supportsClaudeMemory("codex")).toBe(false)
    expect(supportsClaudeMemory(null)).toBe(false)
  })
})
