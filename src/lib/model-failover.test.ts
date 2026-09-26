import { describe, expect, it, vi, afterEach } from "vitest"
import { act, renderHook, waitFor } from "@testing-library/react"

import type { SessionFailureRecord } from "@/lib/types"
import {
  detectLimitTrigger,
  isLimitSessionFailure,
  matchesLimitText,
  useFailoverOffer,
  type PhantomHandoffResponse,
  type PhantomSuccessorResponse,
} from "./model-failover"

// `getTransport()` reaches into the real transport layer (Tauri/Web
// detection), which has no server to answer it under vitest — mock the one
// function this module calls, same pattern as `model-scorecard.test.ts`.
const callMock = vi.fn()
vi.mock("./transport", () => ({
  getTransport: () => ({ call: callMock }),
}))

afterEach(() => {
  callMock.mockReset()
})

function record(
  overrides: Partial<SessionFailureRecord> = {}
): SessionFailureRecord {
  return {
    id: "f1",
    revision: 1,
    category: "unknown",
    severity: "error",
    title: "Something went wrong",
    resolved: false,
    ...overrides,
  }
}

describe("matchesLimitText", () => {
  it("matches the documented limit signals, case-insensitively", () => {
    expect(matchesLimitText("You've hit your session limit · resets 5am")).toBe(
      true
    )
    expect(matchesLimitText("USAGE LIMIT reached")).toBe(true)
    expect(matchesLimitText("rate limit exceeded, try later")).toBe(true)
    expect(matchesLimitText("You are out of quota")).toBe(true)
    expect(matchesLimitText("request failed with status 429")).toBe(true)
    expect(matchesLimitText("account is out of credits")).toBe(true)
    expect(matchesLimitText("spend limit reached for this org")).toBe(true)
    expect(matchesLimitText("error: rate_limit_exceeded")).toBe(true)
    expect(matchesLimitText("insufficient_quota for this key")).toBe(true)
    expect(matchesLimitText("credit balance is too low")).toBe(true)
    expect(matchesLimitText("you exceeded your current plan")).toBe(true)
    expect(matchesLimitText("too many requests, slow down")).toBe(true)
  })

  it("does not match unrelated text", () => {
    expect(matchesLimitText("Connection reset by peer")).toBe(false)
    expect(matchesLimitText("Invalid API key")).toBe(false)
    expect(matchesLimitText(null)).toBe(false)
    expect(matchesLimitText(undefined)).toBe(false)
    expect(matchesLimitText("")).toBe(false)
  })
})

describe("isLimitSessionFailure", () => {
  it('trusts category "limit" at error severity regardless of wording', () => {
    expect(
      isLimitSessionFailure(
        record({ category: "limit", severity: "error", title: "opaque" })
      )
    ).toBe(true)
  })

  it('promotes a "limit"-category warning only when the text says the quota is actually spent', () => {
    // A warning is usually the adapter reconnecting on its own — mirrors the
    // backend's EXHAUSTED_PATTERNS-vs-QUOTA_PATTERNS split.
    expect(
      isLimitSessionFailure(
        record({
          category: "limit",
          severity: "warning",
          title: "You've hit your session limit",
        })
      )
    ).toBe(true)
    // "rate limit" / "429" / "too many requests" are throttling, not spend —
    // they promote an error but not a mere warning.
    expect(
      isLimitSessionFailure(
        record({ category: "limit", severity: "warning", title: "429" })
      )
    ).toBe(false)
    expect(
      isLimitSessionFailure(
        record({ category: "limit", severity: "warning", title: "opaque" })
      )
    ).toBe(false)
  })

  it("falls back to text matching for an error-severity record filed under another category", () => {
    expect(
      isLimitSessionFailure(
        record({
          category: "unknown",
          severity: "error",
          title: "You've hit your session limit",
          details: "resets 5am",
        })
      )
    ).toBe(true)
    expect(
      isLimitSessionFailure(
        record({ category: "connection", severity: "error", title: "429" })
      )
    ).toBe(true)
    expect(
      isLimitSessionFailure(
        record({
          category: "unknown",
          severity: "error",
          title: "insufficient_quota",
        })
      )
    ).toBe(true)
  })

  it('never matches a warning outside category "limit", or a resolved record', () => {
    expect(
      isLimitSessionFailure(
        record({
          category: "unknown",
          severity: "warning",
          title: "rate limit — retrying",
        })
      )
    ).toBe(false)
    expect(
      isLimitSessionFailure(
        record({ category: "limit", resolved: true, severity: "error" })
      )
    ).toBe(false)
  })
})

describe("detectLimitTrigger", () => {
  it("returns null with no failures and no matching error text", () => {
    expect(detectLimitTrigger([], null)).toBeNull()
    expect(detectLimitTrigger(null, "some other error")).toBeNull()
  })

  it("picks the LATEST matching record over earlier ones", () => {
    const failures = [
      record({ id: "a", revision: 1, category: "connection" }),
      record({
        id: "b",
        revision: 2,
        category: "limit",
        title: "Out of tokens",
      }),
    ]
    const trigger = detectLimitTrigger(failures)
    expect(trigger).toEqual({ key: "b@2", reasonText: "Out of tokens" })
  })

  it("falls back to the turn-error text when no record matches", () => {
    const trigger = detectLimitTrigger(
      [record({ category: "connection" })],
      "You've hit your usage limit"
    )
    expect(trigger).toEqual({
      key: "error:You've hit your usage limit",
      reasonText: "You've hit your usage limit",
    })
  })

  it("prefers a matching record over the turn-error text", () => {
    const failures = [record({ category: "limit", title: "Quota hit" })]
    const trigger = detectLimitTrigger(failures, "rate limit exceeded")
    expect(trigger?.key).toBe("f1@1")
  })
})

function successorResponse(
  overrides: Partial<PhantomSuccessorResponse> = {}
): PhantomSuccessorResponse {
  return {
    limited: null,
    successor: {
      agent_type: "open_code",
      model: "opencode/nemotron-3-ultra-free",
      label: "Nemotron 3 Ultra",
      reason: "0.4% tool errors, 62 tok/s",
      metrics: {
        turns: 340,
        tool_error_pct: 0.4,
        output_tokens_per_s: 62,
        context: 262000,
      },
    },
    runner_up: null,
    ...overrides,
  }
}

describe("useFailoverOffer", () => {
  it("fetches nothing while there is no trigger", () => {
    renderHook(() =>
      useFailoverOffer(66, "claude_code", "claude-sonnet-5", null)
    )
    expect(callMock).not.toHaveBeenCalled()
  })

  it("fetches once for a trigger and exposes the successor", async () => {
    callMock.mockResolvedValue(successorResponse())
    const trigger = { key: "f1@1", reasonText: "Out of tokens" }
    const { result } = renderHook(() =>
      useFailoverOffer(66, "claude_code", "claude-sonnet-5", trigger)
    )
    expect(result.current.loading).toBe(true)
    await waitFor(() => expect(result.current.loading).toBe(false))
    expect(result.current.offer?.successor?.agent_type).toBe("open_code")
    expect(callMock).toHaveBeenCalledTimes(1)
    expect(callMock).toHaveBeenCalledWith("phantom_successor", {
      agentType: "claude_code",
      model: "claude-sonnet-5",
      conversationId: 66,
    })
  })

  it("dedups concurrent mounts for the same conversation into one call", async () => {
    let resolveCall: (value: PhantomSuccessorResponse) => void = () => {}
    callMock.mockReturnValue(
      new Promise((resolve) => {
        resolveCall = resolve
      })
    )
    const trigger = { key: "f1@1", reasonText: "Out of tokens" }
    const first = renderHook(() =>
      useFailoverOffer(66, "claude_code", "claude-sonnet-5", trigger)
    )
    const second = renderHook(() =>
      useFailoverOffer(66, "claude_code", "claude-sonnet-5", trigger)
    )
    expect(callMock).toHaveBeenCalledTimes(1)
    act(() => resolveCall(successorResponse()))
    await waitFor(() => expect(first.result.current.loading).toBe(false))
    await waitFor(() => expect(second.result.current.loading).toBe(false))
    expect(first.result.current.offer?.successor).toBeTruthy()
    expect(second.result.current.offer?.successor).toBeTruthy()
  })

  it("dismiss hides the offer for the same trigger key but a new key re-offers", async () => {
    callMock.mockResolvedValue(successorResponse())
    const triggerA = { key: "f1@1", reasonText: "Out of tokens" }
    const { result, rerender } = renderHook(
      ({ trigger }) =>
        useFailoverOffer(66, "claude_code", "claude-sonnet-5", trigger),
      { initialProps: { trigger: triggerA } }
    )
    await waitFor(() => expect(result.current.offer).not.toBeNull())

    act(() => result.current.dismiss())
    expect(result.current.offer).toBeNull()

    // Same key again — stays dismissed, no new fetch.
    rerender({ trigger: { ...triggerA } })
    expect(result.current.offer).toBeNull()
    expect(callMock).toHaveBeenCalledTimes(1)

    // A NEW trigger (escalated revision) re-offers.
    const triggerB = { key: "f1@2", reasonText: "Still out of tokens" }
    rerender({ trigger: triggerB })
    await waitFor(() => expect(result.current.offer).not.toBeNull())
    expect(callMock).toHaveBeenCalledTimes(2)
  })

  it("accept() posts the handoff and surfaces a retryable error on failure", async () => {
    callMock.mockResolvedValueOnce(successorResponse())
    const trigger = { key: "f1@1", reasonText: "Out of tokens" }
    const { result } = renderHook(() =>
      useFailoverOffer(66, "claude_code", "claude-sonnet-5", trigger)
    )
    await waitFor(() => expect(result.current.offer).not.toBeNull())

    const handoffResult: PhantomHandoffResponse = {
      conversation_id: 67,
      folder_id: 3,
      agent_type: "open_code",
      model: "opencode/nemotron-3-ultra-free",
      connection_id: "conn-1",
    }
    callMock.mockResolvedValueOnce(handoffResult)
    let accepted: PhantomHandoffResponse | null = null
    await act(async () => {
      accepted = await result.current.accept()
    })
    expect(accepted).toEqual(handoffResult)
    expect(callMock).toHaveBeenLastCalledWith("phantom_handoff", {
      conversationId: 66,
      targetAgentType: "open_code",
      targetModel: "opencode/nemotron-3-ultra-free",
    })

    callMock.mockRejectedValueOnce(new Error("network down"))
    await act(async () => {
      accepted = await result.current.accept()
    })
    expect(accepted).toBeNull()
    expect(result.current.error).toBe("network down")
  })

  it("accept() can target an explicit candidate (the runner-up)", async () => {
    callMock.mockResolvedValueOnce(
      successorResponse({
        runner_up: {
          agent_type: "codex",
          model: "gpt-5.3-codex",
          label: "Codex",
          reason: "Also measurably fast",
          metrics: { turns: 210 },
        },
      })
    )
    const trigger = { key: "f1@1", reasonText: "Out of tokens" }
    const { result } = renderHook(() =>
      useFailoverOffer(66, "claude_code", "claude-sonnet-5", trigger)
    )
    await waitFor(() => expect(result.current.offer).not.toBeNull())

    callMock.mockResolvedValueOnce({
      conversation_id: 68,
      folder_id: 3,
      agent_type: "codex",
      model: "gpt-5.3-codex",
      connection_id: "conn-2",
    })
    await act(async () => {
      await result.current.accept({
        agent_type: "codex",
        model: "gpt-5.3-codex",
      })
    })
    expect(callMock).toHaveBeenLastCalledWith("phantom_handoff", {
      conversationId: 66,
      targetAgentType: "codex",
      targetModel: "gpt-5.3-codex",
    })
  })
})
