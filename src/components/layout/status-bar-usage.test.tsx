import { fireEvent, render, screen, waitFor } from "@testing-library/react"
import { NextIntlClientProvider } from "next-intl"
import { beforeEach, describe, expect, it, vi } from "vitest"

import type { UsageMeterResponse } from "@/lib/usage-meter"

const mocks = vi.hoisted(() => ({ transportCall: vi.fn() }))

vi.mock("@/lib/transport", () => ({
  getTransport: () => ({ call: mocks.transportCall }),
}))

import { StatusBarUsage } from "./status-bar-usage"
import enMessages from "@/i18n/messages/en.json"

const meter: UsageMeterResponse = {
  claude: {
    plan: "pro",
    five_hour: { percent: 77, resets_at: "2026-10-10T20:00:00Z" },
    seven_day: { percent: 62, resets_at: "2026-10-15T00:00:00Z" },
    fetched_at: "2026-10-10T12:00:00Z",
  },
  claude_error: null,
  today: [
    {
      agent_type: "claude_code",
      model: "claude-sonnet-5",
      input_tokens: 1200,
      output_tokens: 3400,
      turns: 5,
    },
  ],
  week_tokens: 1_200_000,
}

function mount() {
  return render(
    <NextIntlClientProvider locale="en" messages={enMessages}>
      <StatusBarUsage />
    </NextIntlClientProvider>
  )
}

beforeEach(() => {
  mocks.transportCall.mockReset()
})

describe("StatusBarUsage", () => {
  it("renders the compact Claude quota line", async () => {
    mocks.transportCall.mockResolvedValue(meter)
    mount()
    const trigger = await screen.findByRole("button", {
      name: "Token usage",
    })
    expect(trigger).toHaveTextContent("Claude 77% · wk 62%")
    expect(trigger.className).toContain("text-amber-500")
  })

  it("hides itself when there is no data", async () => {
    mocks.transportCall.mockResolvedValue(null)
    const { container } = mount()
    await waitFor(() => expect(mocks.transportCall).toHaveBeenCalled())
    expect(container).toBeEmptyDOMElement()
  })

  it("shows the quota windows and today's tokens in the popover", async () => {
    mocks.transportCall.mockResolvedValue(meter)
    mount()
    fireEvent.click(await screen.findByRole("button", { name: "Token usage" }))

    expect(await screen.findByText("5 h")).toBeVisible()
    expect(screen.getByText("Week")).toBeVisible()
    expect(screen.getAllByText(/Resets at/)).toHaveLength(2)
    expect(screen.getByText("Today")).toBeVisible()
    expect(screen.getByText("Claude Code · claude-sonnet-5")).toBeVisible()
    expect(screen.getByText("4.6k")).toBeVisible()
    expect(screen.getByText("This week: 1.2M")).toBeVisible()
  })

  it("shows the Claude read error while still listing today's tokens", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      claude: null,
      claude_error: "no OAuth credentials",
    })
    const { container } = mount()
    const trigger = await screen.findByRole("button", {
      name: "Token usage",
    })
    expect(trigger).toHaveTextContent("4.6k today")
    fireEvent.click(trigger)
    expect(
      await screen.findByText(
        "Couldn't read Claude usage: no OAuth credentials"
      )
    ).toBeVisible()
    // Rendered once the data arrives, not before.
    expect(container).not.toBeEmptyDOMElement()
  })
})
