import { describe, expect, it } from "vitest"

import { formatTokens, usageLevel } from "./usage-meter"

describe("usageLevel", () => {
  it("is green below 70%", () => {
    expect(usageLevel(0)).toBe("ok")
    expect(usageLevel(69.9)).toBe("ok")
  })

  it("is amber from 70% up to (but not including) 90%", () => {
    expect(usageLevel(70)).toBe("warn")
    expect(usageLevel(89.9)).toBe("warn")
  })

  it("is red at 90% and above", () => {
    expect(usageLevel(90)).toBe("high")
    expect(usageLevel(100)).toBe("high")
  })

  it("treats an unreadable number as green", () => {
    expect(usageLevel(Number.NaN)).toBe("ok")
  })
})

describe("formatTokens", () => {
  it("prints plain numbers under a thousand", () => {
    expect(formatTokens(0)).toBe("0")
    expect(formatTokens(999)).toBe("999")
  })

  it("abbreviates thousands with a k", () => {
    expect(formatTokens(1000)).toBe("1k")
    expect(formatTokens(1200)).toBe("1.2k")
    expect(formatTokens(12_300)).toBe("12.3k")
  })

  it("abbreviates millions with an M", () => {
    expect(formatTokens(1_000_000)).toBe("1M")
    expect(formatTokens(3_400_000)).toBe("3.4M")
  })

  it("clamps invalid input to zero", () => {
    expect(formatTokens(-5)).toBe("0")
    expect(formatTokens(Number.NaN)).toBe("0")
  })
})
