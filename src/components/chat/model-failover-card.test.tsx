import { fireEvent, render, screen } from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { describe, expect, it, vi } from "vitest"

import { ModelFailoverCard } from "./model-failover-card"
import enMessages from "@/i18n/messages/en.json"
import type { PhantomSuccessorResponse } from "@/lib/model-failover"

function renderCard(
  props: Partial<React.ComponentProps<typeof ModelFailoverCard>> = {}
) {
  return render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <ModelFailoverCard
        limitedAgentType="claude_code"
        reasonText="You've hit your session limit"
        offer={null}
        loading={false}
        accepting={false}
        error={null}
        onAccept={vi.fn()}
        onDismiss={vi.fn()}
        onRetry={vi.fn()}
        {...props}
      />
    </NextIntlClientProvider>
  )
}

const successorOffer: PhantomSuccessorResponse = {
  limited: {
    agent_type: "claude_code",
    scope: "account",
    model: "claude-sonnet-5",
    message: "You've hit your session limit",
    hit_at: "2026-09-25T12:00:00Z",
    resets_hint: "resets 5am",
    resets_at: null,
  },
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
  runner_up: {
    agent_type: "codex",
    model: "gpt-5.3-codex",
    label: "Codex",
    reason: "Also measurably fast",
    metrics: {
      turns: 210,
      tool_error_pct: null,
      output_tokens_per_s: null,
      context: null,
    },
  },
}

describe("ModelFailoverCard", () => {
  it("is an accessible status region announcing the limit with the reset hint", () => {
    renderCard({ offer: successorOffer })
    const region = screen.getByRole("status")
    expect(region).toHaveTextContent("Claude Code ran out of tokens")
    expect(region).toHaveTextContent("resets 5am")
    expect(region).toHaveTextContent("You've hit your session limit")
  })

  it("shows the loading state before the successor lookup resolves", () => {
    renderCard({ loading: true })
    expect(screen.getByText("Looking for the best successor…")).toBeVisible()
  })

  it("offers the successor chip and a primary accept action", () => {
    const onAccept = vi.fn()
    renderCard({ offer: successorOffer, onAccept })
    expect(screen.getAllByText(/Nemotron 3 Ultra/).length).toBeGreaterThan(0)
    fireEvent.click(
      screen.getByRole("button", {
        name: /Continue with OpenCode · Nemotron 3 Ultra/,
      })
    )
    expect(onAccept).toHaveBeenCalledWith()
  })

  it('reveals and accepts the runner-up on "Choose another"', () => {
    const onAccept = vi.fn()
    renderCard({ offer: successorOffer, onAccept })
    fireEvent.click(screen.getByText("Choose another"))
    fireEvent.click(screen.getByText("Use this one"))
    expect(onAccept).toHaveBeenCalledWith({
      agent_type: "codex",
      model: "gpt-5.3-codex",
    })
  })

  it("wait dismisses without accepting", () => {
    const onDismiss = vi.fn()
    renderCard({ offer: successorOffer, onDismiss })
    fireEvent.click(screen.getByText("Wait"))
    expect(onDismiss).toHaveBeenCalledTimes(1)
  })

  it("says there is no successor yet, with the reset hint, when the offer has none", () => {
    renderCard({
      offer: { ...successorOffer, successor: null, runner_up: null },
    })
    expect(
      screen.getByText("No measured successor available yet (resets 5am).")
    ).toBeVisible()
  })

  it("shows a retryable inline error", () => {
    const onRetry = vi.fn()
    renderCard({ offer: successorOffer, error: "network down", onRetry })
    expect(screen.getByText("Couldn't prepare the handoff.")).toBeVisible()
    fireEvent.click(screen.getByText("Retry"))
    expect(onRetry).toHaveBeenCalledTimes(1)
  })

  it("disables actions while accepting", () => {
    renderCard({ offer: successorOffer, accepting: true })
    const acceptButton = screen.getByRole("button", {
      name: /Continue with OpenCode · Nemotron 3 Ultra/,
    })
    expect(acceptButton).toBeDisabled()
    expect(screen.getByText("Wait")).toBeDisabled()
  })
})
