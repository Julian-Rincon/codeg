import { describe, expect, it, vi, afterEach } from "vitest"
import type { AgentType } from "@/lib/types"

import {
  buildAgentMentionHints,
  buildScorecardMetricLine,
  fetchModelScorecard,
  findModelScore,
  formatBestForMetric,
  formatScorecardContext,
  formatScorecardPercent,
  formatScorecardSeconds,
  formatScorecardTokensPerSecond,
  hasScorecardData,
  scorecardStatusLine,
  scorecardStrengthLabels,
  type AnyTranslate,
  type ModelScorecardBestFor,
  type ModelScorecardEntry,
  type ModelScorecardResponse,
  type ScorecardTranslate,
} from "./model-scorecard"

// `getTransport()` reaches into the real transport layer (Tauri/Web
// detection), which has no server to answer it under vitest — mock the one
// function `fetchModelScorecard` calls.
const callMock = vi.fn()
vi.mock("./transport", () => ({
  getTransport: () => ({ call: callMock }),
}))

function makeEntry(
  overrides: Partial<ModelScorecardEntry> = {}
): ModelScorecardEntry {
  return {
    agent_type: "claude_code",
    model: "claude-sonnet-5",
    label: "Sonnet 5",
    available: true,
    conversations: 12,
    turns: 13259,
    avg_turn_ms: 14200,
    p50_turn_ms: 9000,
    output_tokens_per_s: 56.2,
    output_tokens_per_turn: 800.5,
    cache_hit_pct: 97.7,
    tool_calls: 5000,
    tool_error_pct: 3.1,
    category_calls: { edit: 0, read: 0, shell: 0, web: 0, agent: 0 },
    category_error_pct: {
      edit: null,
      read: null,
      shell: null,
      web: null,
      agent: null,
    },
    last_used_at: "2026-09-24T00:00:00Z",
    spec: {
      context: 262144,
      reasoning: true,
      tool_call: true,
      cost_in: 0,
      cost_out: 0,
    },
    strengths: ["edit", "fast"],
    low_sample: false,
    ...overrides,
  }
}

function makeScorecard(models: ModelScorecardEntry[]): ModelScorecardResponse {
  return { generated_at: "2026-09-24T00:00:00Z", models, best_for: [] }
}

describe("fetchModelScorecard", () => {
  afterEach(() => {
    vi.clearAllMocks()
  })

  it("returns the parsed response on success", async () => {
    const scorecard = makeScorecard([makeEntry()])
    callMock.mockResolvedValueOnce(scorecard)
    await expect(fetchModelScorecard()).resolves.toEqual(scorecard)
    expect(callMock).toHaveBeenCalledWith("model_scorecard", {})
  })

  it("returns null instead of throwing on a 404 (old server)", async () => {
    callMock.mockRejectedValueOnce({ code: "not_found", message: "HTTP 404" })
    await expect(fetchModelScorecard()).resolves.toBeNull()
  })

  it("returns null on a network error", async () => {
    callMock.mockRejectedValueOnce(new Error("network error"))
    await expect(fetchModelScorecard()).resolves.toBeNull()
  })
})

describe("findModelScore", () => {
  const scorecard = makeScorecard([
    makeEntry({ agent_type: "claude_code", model: "claude-sonnet-5" }),
    makeEntry({
      agent_type: "claude_code",
      model: "claude-opus-5-5",
      label: "Opus 5.5",
    }),
    makeEntry({
      agent_type: "open_code",
      model: "nemotron-3.5-lightning-free",
      label: "Nemotron 3.5 Lightning",
    }),
    makeEntry({
      agent_type: "hermes",
      model: "gpt-4o",
      turns: 1,
      low_sample: true,
    }),
  ])

  it("returns null for a null scorecard", () => {
    expect(findModelScore(null, "claude_code", "claude-sonnet-5")).toBeNull()
  })

  it("matches by exact bare id", () => {
    const entry = findModelScore(scorecard, "claude_code", "claude-sonnet-5")
    expect(entry?.model).toBe("claude-sonnet-5")
  })

  it("matches after stripping a provider/ prefix", () => {
    const entry = findModelScore(
      scorecard,
      "open_code",
      "opencode/nemotron-3.5-lightning-free"
    )
    expect(entry?.model).toBe("nemotron-3.5-lightning-free")
  })

  it("matches by the last path segment when the prefix has multiple parts", () => {
    const entry = findModelScore(
      scorecard,
      "open_code",
      "openrouter/opencode/nemotron-3.5-lightning-free"
    )
    expect(entry?.model).toBe("nemotron-3.5-lightning-free")
  })

  it("matches by display label when the value is an unrelated alias", () => {
    const entry = findModelScore(scorecard, "claude_code", "opus", "Opus 5.5")
    expect(entry?.model).toBe("claude-opus-5-5")
  })

  it("never crosses agent_type boundaries", () => {
    // `gpt-4o` only exists under hermes; asking under claude_code must miss.
    expect(findModelScore(scorecard, "claude_code", "gpt-4o")).toBeNull()
  })

  it("returns null for a genuine alias with no scorecard match", () => {
    expect(findModelScore(scorecard, "claude_code", "default")).toBeNull()
  })

  it("still returns a low_sample entry — callers decide how to render it", () => {
    const entry = findModelScore(scorecard, "hermes", "gpt-4o")
    expect(entry?.low_sample).toBe(true)
  })
})

describe("hasScorecardData", () => {
  it("is false for null/undefined", () => {
    expect(hasScorecardData(null)).toBe(false)
    expect(hasScorecardData(undefined)).toBe(false)
  })

  it("is false for low_sample entries", () => {
    expect(hasScorecardData(makeEntry({ low_sample: true }))).toBe(false)
  })

  it("is false for zero turns", () => {
    expect(hasScorecardData(makeEntry({ turns: 0 }))).toBe(false)
  })

  it("is true for a real entry", () => {
    expect(hasScorecardData(makeEntry())).toBe(true)
  })
})

describe("formatters", () => {
  it("formatScorecardSeconds rounds to whole seconds at/above 10s", () => {
    expect(formatScorecardSeconds(14200)).toBe("14")
  })

  it("formatScorecardSeconds keeps a decimal under 10s", () => {
    expect(formatScorecardSeconds(900)).toBe("0.9")
  })

  it("formatScorecardSeconds is null for missing data", () => {
    expect(formatScorecardSeconds(null)).toBeNull()
    expect(formatScorecardSeconds(undefined)).toBeNull()
  })

  it("formatScorecardTokensPerSecond rounds like seconds", () => {
    expect(formatScorecardTokensPerSecond(56.2)).toBe("56")
    expect(formatScorecardTokensPerSecond(4.3)).toBe("4.3")
  })

  it("formatScorecardPercent keeps a decimal under 10", () => {
    expect(formatScorecardPercent(3.1)).toBe("3.1")
    expect(formatScorecardPercent(12.6)).toBe("13")
  })

  it("formatScorecardPercent respects locale decimal separators", () => {
    expect(formatScorecardPercent(1.2, "es")).toBe("1,2")
  })

  it("formatScorecardContext compacts thousands and millions", () => {
    expect(formatScorecardContext(262144)).toBe("262k")
    expect(formatScorecardContext(1_000_000)).toBe("1M")
    expect(formatScorecardContext(200_000)).toBe("200k")
    expect(formatScorecardContext(500)).toBe("500")
  })

  it("formatScorecardContext is null for missing/zero context", () => {
    expect(formatScorecardContext(null)).toBeNull()
    expect(formatScorecardContext(0)).toBeNull()
  })
})

describe("buildScorecardMetricLine", () => {
  const t: ScorecardTranslate = (key, values) => {
    switch (key) {
      case "metricDuration":
        return `${values.value}s/turn`
      case "metricTokPerSec":
        return `${values.value} tok/s`
      case "metricErrorPct":
        return `${values.value}% errors`
      case "metricContext":
        return `${values.value} ctx`
    }
  }

  it("joins every available segment with a middle dot", () => {
    const line = buildScorecardMetricLine(makeEntry(), t)
    expect(line).toBe("14s/turn · 56 tok/s · 3.1% errors · 262k ctx")
  })

  it("omits a segment whose underlying measurement is missing", () => {
    const line = buildScorecardMetricLine(
      makeEntry({ spec: null, tool_error_pct: null }),
      t
    )
    expect(line).toBe("14s/turn · 56 tok/s")
  })

  it("returns null when nothing at all is measured", () => {
    const line = buildScorecardMetricLine(
      makeEntry({
        avg_turn_ms: null,
        output_tokens_per_s: null,
        tool_error_pct: null,
        spec: null,
      }),
      t
    )
    expect(line).toBeNull()
  })
})

describe("scorecardStrengthLabels", () => {
  const anyT: AnyTranslate = (key) => `[${key}]`

  it("maps each strength category to its i18n key", () => {
    expect(
      scorecardStrengthLabels(makeEntry({ strengths: ["edit", "fast"] }), anyT)
    ).toEqual(["[categoryEdit]", "[categoryFast]"])
  })

  it("is empty when the model has no measured strengths", () => {
    expect(scorecardStrengthLabels(makeEntry({ strengths: [] }), anyT)).toEqual(
      []
    )
  })
})

describe("formatBestForMetric", () => {
  const anyT: AnyTranslate = (key, values) => {
    if (!values) return key
    switch (key) {
      case "metricErrorPct":
        return `${values.value}% errors`
      case "metricTokPerSec":
        return `${values.value} tok/s`
      case "metricContext":
        return `${values.value} ctx`
      default:
        return key
    }
  }

  it("formats an error_pct win", () => {
    expect(formatBestForMetric({ metric: "error_pct", value: 1.2 }, anyT)).toBe(
      "1.2% errors"
    )
  })

  it("formats an output_tokens_per_s win", () => {
    expect(
      formatBestForMetric({ metric: "output_tokens_per_s", value: 14.3 }, anyT)
    ).toBe("14 tok/s")
  })

  it("formats a context win", () => {
    expect(
      formatBestForMetric({ metric: "context", value: 262144 }, anyT)
    ).toBe("262k ctx")
  })
})

describe("buildAgentMentionHints", () => {
  const anyT: AnyTranslate = (key, values) => {
    if (key === "categoryEdit") return "Edit"
    if (key === "categoryFree") return "Free"
    if (!values) return key
    switch (key) {
      case "metricErrorPct":
        return `${values.value}% errors`
      case "metricTokPerSec":
        return `${values.value} tok/s`
      case "mentionHint":
        return `${values.agent} · ${values.model} — best at ${values.category} (${values.metric}, n=${values.sample})`
      default:
        return key
    }
  }

  const bestFor: ModelScorecardBestFor[] = [
    {
      category: "edit",
      agent_type: "claude_code",
      model: "claude-opus-5-5",
      metric: "error_pct",
      value: 1.2,
      sample: 340,
      runner_up: null,
    },
    {
      // Same agent, smaller sample — must lose to the "edit" win above.
      category: "fast",
      agent_type: "claude_code",
      model: "claude-sonnet-5",
      metric: "output_tokens_per_s",
      value: 56.2,
      sample: 50,
      runner_up: null,
    },
    {
      category: "free",
      agent_type: "open_code",
      model: "space-bunny-free",
      metric: "output_tokens_per_s",
      value: 14.3,
      sample: 416,
      runner_up: null,
    },
  ]

  const scorecard = makeScorecard([
    makeEntry({
      agent_type: "claude_code",
      model: "claude-opus-5-5",
      label: "Opus 5.5",
    }),
    // No scorecard entry for "space-bunny-free" — the hint must still fall
    // back to the bare model id from `best_for` rather than dropping it.
  ])
  scorecard.best_for = bestFor

  const agentLabels = new Map([
    ["claude_code", "Claude Code"],
    ["open_code", "OpenCode"],
  ])

  it("picks the highest-sample category per agent", () => {
    const hints = buildAgentMentionHints(scorecard, agentLabels, anyT)
    expect(hints.get("claude_code")).toBe(
      "Claude Code · Opus 5.5 — best at Edit (1.2% errors, n=340)"
    )
  })

  it("falls back to the bare model id when it has no scorecard label", () => {
    const hints = buildAgentMentionHints(scorecard, agentLabels, anyT)
    expect(hints.get("open_code")).toBe(
      "OpenCode · space-bunny-free — best at Free (14 tok/s, n=416)"
    )
  })

  it("returns an empty map for a null scorecard", () => {
    expect(buildAgentMentionHints(null, agentLabels, anyT).size).toBe(0)
  })

  it("never invents a hint for an agent with no best_for win", () => {
    const hints = buildAgentMentionHints(scorecard, agentLabels, anyT)
    expect(hints.has("hermes")).toBe(false)
  })
})

describe("scorecardStatusLine", () => {
  const anyT: AnyTranslate = (key, values) => {
    if (key === "noData") return "Not enough data yet"
    if (!values) return key
    switch (key) {
      case "metricDuration":
        return `${values.value}s/turn`
      case "metricTokPerSec":
        return `${values.value} tok/s`
      case "metricErrorPct":
        return `${values.value}% errors`
      case "metricContext":
        return `${values.value} ctx`
      default:
        return key
    }
  }

  it("returns the metric line for an entry with real data", () => {
    expect(scorecardStatusLine(makeEntry(), anyT)).toBe(
      "14s/turn · 56 tok/s · 3.1% errors · 262k ctx"
    )
  })

  it("returns the no-data string for a low_sample entry", () => {
    expect(scorecardStatusLine(makeEntry({ low_sample: true }), anyT)).toBe(
      "Not enough data yet"
    )
  })

  it("returns the no-data string for zero turns", () => {
    expect(scorecardStatusLine(makeEntry({ turns: 0 }), anyT)).toBe(
      "Not enough data yet"
    )
  })
})

describe("findModelScore label slugs", () => {
  const card = {
    generated_at: "2026-09-25T00:00:00Z",
    best_for: [],
    models: [
      { agent_type: "claude_code", model: "claude-opus-5", label: null },
      { agent_type: "claude_code", model: "claude-opus-5-5", label: null },
      { agent_type: "claude_code", model: "claude-sonnet-5", label: null },
    ],
  } as unknown as Parameters<typeof findModelScore>[0]

  it("resolves alias options through their display label", () => {
    expect(
      findModelScore(card, "claude_code" as AgentType, "sonnet", "Sonnet 5")
        ?.model
    ).toBe("claude-sonnet-5")
    expect(
      findModelScore(card, "claude_code" as AgentType, "opus", "Opus 5.5")
        ?.model
    ).toBe("claude-opus-5-5")
  })

  it("never lets a shorter slug claim a longer version", () => {
    expect(
      findModelScore(card, "claude_code" as AgentType, "opus", "Opus 5")?.model
    ).toBe("claude-opus-5")
    expect(
      findModelScore(card, "claude_code" as AgentType, "haiku", "Haiku 4.5")
    ).toBeNull()
  })
})

describe("findModelScore with live catalog aliases", () => {
  it("prefers the measured entry over a zero-usage alias with the same id", () => {
    const card = {
      generated_at: "2026-09-25T00:00:00Z",
      best_for: [],
      models: [
        {
          agent_type: "claude_code",
          model: "sonnet",
          label: "Sonnet 5",
          turns: 0,
          low_sample: true,
          available: true,
        },
        {
          agent_type: "claude_code",
          model: "claude-sonnet-5",
          label: null,
          turns: 13259,
          low_sample: false,
          available: null,
        },
      ],
    } as unknown as Parameters<typeof findModelScore>[0]
    expect(
      findModelScore(card, "claude_code" as AgentType, "sonnet", "Sonnet 5")
        ?.model
    ).toBe("claude-sonnet-5")
  })
})
