import { describe, expect, it } from "vitest"
import {
  findRewindForkTarget,
  isCheckpointBusyError,
  planConversationRewind,
  summarizeChangedFiles,
  type CheckpointChangedFile,
  type RewindThreadItem,
} from "./phantom-checkpoints"
import type { MessageTurn } from "./types"

function turn(id: string, sourceTurnId?: string): MessageTurn {
  return {
    id,
    role: "assistant",
    blocks: [],
    timestamp: "2026-10-28T12:00:00Z",
    ...(sourceTurnId ? { source_turn_id: sourceTurnId } : {}),
  }
}

function assistant(key: string, turns: MessageTurn[]): RewindThreadItem {
  return {
    key,
    kind: "turn",
    group: { role: "assistant" },
    sourceTurns: turns,
  }
}

const user: RewindThreadItem = {
  key: "m1",
  kind: "turn",
  group: { role: "user" },
}

describe("summarizeChangedFiles", () => {
  it("adds per-file line counts and counts files beyond the list", () => {
    const files: CheckpointChangedFile[] = [
      { path: "a.ts", status: "modified", additions: 2, deletions: 3 },
      { path: "bin.png", status: "added", additions: null, deletions: null },
      { path: "gone.ts", status: "deleted", additions: 0, deletions: 9 },
    ]
    expect(summarizeChangedFiles(files, 4)).toEqual({
      files: 7,
      additions: 2,
      deletions: 12,
    })
  })

  it("is all zeroes for an unchanged tree", () => {
    expect(summarizeChangedFiles([])).toEqual({
      files: 0,
      additions: 0,
      deletions: 0,
    })
  })
})

describe("findRewindForkTarget", () => {
  it("forks at the agent's name for the preceding reply", () => {
    const items = [assistant("a0", [turn("live-1-2", "turn-7")]), user]
    expect(findRewindForkTarget(items, "m1", false)).toEqual({
      kind: "fork",
      turnId: "turn-7",
    })
  })

  it("prefers the last turn of a multi-turn group", () => {
    const items = [assistant("a0", [turn("turn-3"), turn("turn-4")]), user]
    expect(findRewindForkTarget(items, "m1", false)).toEqual({
      kind: "fork",
      turnId: "turn-4",
    })
  })

  it("reports a reply streamed here but not named by the agent yet", () => {
    const items = [assistant("a0", [turn("live-2-9")]), user]
    expect(findRewindForkTarget(items, "m1", false)).toEqual({
      kind: "unnamed",
    })
  })

  it("skips user groups and stops at the one reply before the message", () => {
    const items = [
      assistant("a0", [turn("turn-1")]),
      { key: "m0", kind: "turn", group: { role: "user" } } as RewindThreadItem,
      assistant("a1", [turn("turn-2")]),
      user,
    ]
    expect(findRewindForkTarget(items, "m1", false)).toEqual({
      kind: "fork",
      turnId: "turn-2",
    })
  })

  it("is the first message when no reply precedes it", () => {
    expect(findRewindForkTarget([user], "m1", false)).toEqual({ kind: "first" })
    expect(findRewindForkTarget([user], "m1", true)).toEqual({
      kind: "unloaded",
    })
  })

  it("reports unloaded history when the item is not rendered", () => {
    expect(findRewindForkTarget([], "m1", true)).toEqual({ kind: "unloaded" })
  })
})

describe("planConversationRewind", () => {
  it("forks when the agent can", () => {
    expect(
      planConversationRewind({ kind: "fork", turnId: "turn-2" }, true)
    ).toEqual({ mode: "fork", turnId: "turn-2" })
  })

  it("falls back to a fresh conversation when forking is unsupported", () => {
    expect(
      planConversationRewind({ kind: "fork", turnId: "turn-2" }, false)
    ).toEqual({ mode: "fresh", reason: "unsupported" })
    expect(planConversationRewind({ kind: "unnamed" }, false)).toEqual({
      mode: "fresh",
      reason: "unsupported",
    })
  })

  it("opens a fresh conversation for an unnamed reply or the first message", () => {
    expect(planConversationRewind({ kind: "unnamed" }, true)).toEqual({
      mode: "fresh",
      reason: "unnamed",
    })
    expect(planConversationRewind({ kind: "first" }, true)).toEqual({
      mode: "fresh",
      reason: "first",
    })
  })

  it("blocks while older history is not loaded", () => {
    expect(planConversationRewind({ kind: "unloaded" }, true)).toEqual({
      mode: "blocked",
      reason: "unloaded",
    })
  })
})

describe("isCheckpointBusyError", () => {
  it("recognizes the backend rejection on both transports", () => {
    expect(isCheckpointBusyError("turn already in progress")).toBe(true)
    expect(
      isCheckpointBusyError({
        code: "turn_in_progress",
        message: "a turn is running",
      })
    ).toBe(true)
    expect(
      isCheckpointBusyError({
        message: "Error: turn already in progress for this connection",
      })
    ).toBe(true)
  })

  it("leaves genuine failures alone", () => {
    expect(isCheckpointBusyError(new Error("git failed"))).toBe(false)
    expect(isCheckpointBusyError(null)).toBe(false)
  })
})
