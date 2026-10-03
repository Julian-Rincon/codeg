import { describe, expect, it } from "vitest"
import { normaliseUtterance, shouldSendUtterance } from "./utterance-filter"

const base = {
  text: "revisa si el sistema ya está actualizado",
  language: "es",
  durationMs: 4500,
  agentBusy: false,
  expectedLang: "es" as const,
}

describe("shouldSendUtterance", () => {
  it("sends a real request", () => {
    expect(shouldSendUtterance(base)).toBe(true)
  })

  it("drops Whisper's stock hallucinations", () => {
    for (const text of [
      "Thank you.",
      "Gracias.",
      "you",
      "Subtítulos realizados por la comunidad de Amara.org",
    ]) {
      expect(shouldSendUtterance({ ...base, text })).toBe(false)
    }
  })

  // The 2026-10-02 incident: two ~1 s "English" bursts while Claude was
  // working cancelled the turn before it spoke.
  it("drops a short burst in another language", () => {
    expect(
      shouldSendUtterance({
        ...base,
        text: "Oh, right.",
        language: "en",
        durationMs: 1080,
      })
    ).toBe(false)
  })

  it("needs a real sentence to interrupt a working agent", () => {
    expect(shouldSendUtterance({ ...base, text: "sí", agentBusy: true })).toBe(
      false
    )
    expect(
      shouldSendUtterance({
        ...base,
        text: "para eso ya",
        agentBusy: true,
        durationMs: 900,
      })
    ).toBe(false)
    expect(
      shouldSendUtterance({
        ...base,
        text: "para, mejor revisa solo flatpak",
        agentBusy: true,
      })
    ).toBe(true)
  })

  it("still accepts a short reply when the agent is idle", () => {
    expect(
      shouldSendUtterance({ ...base, text: "sí, dale", durationMs: 800 })
    ).toBe(true)
  })

  it("accepts a longer sentence in the other language", () => {
    expect(
      shouldSendUtterance({
        ...base,
        text: "check the system updates please",
        language: "en",
      })
    ).toBe(true)
  })
})

describe("normaliseUtterance", () => {
  it("strips punctuation and case", () => {
    expect(normaliseUtterance("  ¡Gracias!  ")).toBe("gracias")
  })
})
