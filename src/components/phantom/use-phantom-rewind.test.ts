import { describe, expect, it } from "vitest"

import { consumeRewindDraft, parkRewindDraft } from "./use-phantom-rewind"

describe("parked rewind drafts", () => {
  it("hands the text to the tab once it holds the promised agent and folder", () => {
    parkRewindDraft("tab-a", {
      text: "fix the login",
      agentType: "claude_code",
      folderId: 3,
    })
    // A reused draft tab retargets asynchronously: text must wait for it.
    expect(
      consumeRewindDraft("tab-a", { agentType: "codex", folderId: 3 })
    ).toBeNull()
    expect(
      consumeRewindDraft("tab-a", { agentType: null, folderId: null })
    ).toBeNull()
    expect(
      consumeRewindDraft("tab-a", { agentType: "claude_code", folderId: 3 })
    ).toBe("fix the login")
    // Delivered once.
    expect(
      consumeRewindDraft("tab-a", { agentType: "claude_code", folderId: 3 })
    ).toBeNull()
  })

  it("does not leak a draft into another tab", () => {
    parkRewindDraft("tab-b", {
      text: "hello",
      agentType: "claude_code",
      folderId: 1,
    })
    expect(
      consumeRewindDraft("tab-c", { agentType: "claude_code", folderId: 1 })
    ).toBeNull()
    expect(
      consumeRewindDraft("tab-b", { agentType: "claude_code", folderId: 1 })
    ).toBe("hello")
  })
})
