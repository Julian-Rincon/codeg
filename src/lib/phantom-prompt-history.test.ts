import { beforeEach, describe, expect, it } from "vitest"
import {
  addPromptHistoryEntry,
  canOpenPromptSearch,
  isPromptSearchShortcut,
  loadPromptHistory,
  loadRecallHistory,
  promptSearchKeyAction,
  rankPromptHistory,
  recallHistory,
  recordPrompt,
  searchPromptHistory,
  type PromptHistoryEntry,
} from "./phantom-prompt-history"

const e = (text: string, at: number, folder: string | null = null) => ({
  text,
  at,
  folder,
})

describe("addPromptHistoryEntry", () => {
  it("moves a repeated prompt to the front and trims it", () => {
    const out = addPromptHistoryEntry(
      [e("a", 1), e("b", 2)],
      e("  b ", 3, "/x")
    )
    expect(out).toEqual([e("b", 3, "/x"), e("a", 1)])
  })
  it("ignores blank prompts and caps the list", () => {
    expect(addPromptHistoryEntry([e("a", 1)], e("   ", 2))).toEqual([e("a", 1)])
    expect(
      addPromptHistoryEntry([e("a", 1), e("b", 2)], e("c", 3), 2)
    ).toHaveLength(2)
  })
})

describe("rankPromptHistory / recallHistory", () => {
  const entries: PromptHistoryEntry[] = [
    e("old-here", 1, "/repo"),
    e("new-elsewhere", 5, "/other"),
    e("new-here", 4, "/repo"),
  ]
  it("puts the current folder first, each group newest first", () => {
    expect(rankPromptHistory(entries, "/repo").map((x) => x.text)).toEqual([
      "new-here",
      "old-here",
      "new-elsewhere",
    ])
    expect(rankPromptHistory(entries, null)[0].text).toBe("new-elsewhere")
  })
  it("returns oldest-first with the session's prompts newest and no duplicates", () => {
    expect(recallHistory(["s1", "new-here"], entries, "/repo")).toEqual([
      "new-elsewhere",
      "old-here",
      "s1",
      "new-here",
    ])
  })
})

describe("searchPromptHistory", () => {
  const ranked = ["fix the build", "deploy to prod", "build docs"]
  it("ranks prefix, then substring, then fuzzy", () => {
    expect(searchPromptHistory(ranked, "build")).toEqual([
      "build docs",
      "fix the build",
    ])
    expect(searchPromptHistory(ranked, "dtp")).toEqual(["deploy to prod"])
    expect(searchPromptHistory(ranked, "zzz")).toEqual([])
  })
  it("returns everything for an empty query", () => {
    expect(searchPromptHistory(ranked, "  ")).toEqual(ranked)
  })
})

describe("storage", () => {
  beforeEach(() => localStorage.clear())
  it("records and reloads prompts, skipping garbage", () => {
    recordPrompt("hola", "/repo", 10)
    recordPrompt("adiós", null, 20)
    expect(loadPromptHistory().map((x) => x.text)).toEqual(["adiós", "hola"])
    expect(loadRecallHistory([], "/repo")).toEqual(["adiós", "hola"])
    localStorage.setItem("phantom:prompt-history:v1", "{not json")
    expect(loadPromptHistory()).toEqual([])
  })
})

describe("keys", () => {
  it("opens on plain Ctrl+R only", () => {
    expect(isPromptSearchShortcut({ key: "r", ctrlKey: true })).toBe(true)
    expect(
      isPromptSearchShortcut({ key: "R", ctrlKey: true, shiftKey: true })
    ).toBe(false)
    expect(isPromptSearchShortcut({ key: "r", metaKey: true })).toBe(false)
    expect(
      isPromptSearchShortcut({ key: "r", ctrlKey: true, isComposing: true })
    ).toBe(false)
  })
  it("does not open over another menu or a queue edit", () => {
    const base = {
      slashMenuOpen: false,
      mentionMenuOpen: false,
      editingQueueItem: false,
    }
    expect(canOpenPromptSearch(base)).toBe(true)
    expect(canOpenPromptSearch({ ...base, slashMenuOpen: true })).toBe(false)
    expect(canOpenPromptSearch({ ...base, editingQueueItem: true })).toBe(false)
  })
  it("maps keys inside the search box", () => {
    expect(promptSearchKeyAction({ key: "Escape" }, 0, 3)).toEqual({
      type: "close",
    })
    expect(promptSearchKeyAction({ key: "Enter" }, 0, 0)).toBeNull()
    expect(promptSearchKeyAction({ key: "ArrowUp" }, 0, 3)).toEqual({
      type: "move",
      index: 2,
    })
    expect(promptSearchKeyAction({ key: "r", ctrlKey: true }, 2, 3)).toEqual({
      type: "move",
      index: 0,
    })
    expect(promptSearchKeyAction({ key: "a" }, 0, 3)).toBeNull()
  })
})
