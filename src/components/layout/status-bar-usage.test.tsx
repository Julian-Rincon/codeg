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
  router_profile: "calidad",
}

/** Resolve the transport by command name so a test can vary one call's answer. */
function transportCalls(command: string) {
  return mocks.transportCall.mock.calls.filter(([name]) => name === command)
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

describe("StatusBarUsage stale Claude quota", () => {
  const minutesFromNow = (minutes: number) =>
    new Date(Date.now() + minutes * 60_000).toISOString()

  async function openPopover() {
    const trigger = await screen.findByRole("button", { name: "Token usage" })
    fireEvent.click(trigger)
    return trigger
  }

  it("keeps the last good bars, dimmed, when rate limited", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      claude: { ...meter.claude!, fetched_at: minutesFromNow(-5) },
      claude_error: "claude usage http 429",
      claude_stale: true,
      claude_error_kind: "rate_limited",
      claude_retry_at: minutesFromNow(3),
    })
    mount()
    const trigger = await openPopover()
    // The compact line keeps the last good numbers, not the tokens fallback.
    expect(trigger).toHaveTextContent("Claude 77% · wk 62%")
    expect(trigger.className).toContain("opacity-70")

    expect(await screen.findByText("5 h")).toBeVisible()
    expect(screen.getByTestId("usage-quota-bars").className).toContain(
      "opacity-50"
    )
    expect(screen.getByText(/^Updated 5 min\. ago$/)).toBeVisible()
    const status = screen.getByText(
      /^Claude rate-limited the request; retrying in 3 min\.$/
    )
    expect(status.className).toContain("text-amber-500")
    expect(status.className).not.toContain("text-destructive")
    expect(screen.queryByText(/claude usage http 429/)).not.toBeInTheDocument()
  })

  it("says the data is the last reading on a network failure", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      claude_error: "claude usage request failed",
      claude_stale: true,
      claude_error_kind: "network",
      claude_retry_at: null,
    })
    mount()
    await openPopover()
    const status = await screen.findByText(
      "Can't reach Claude; showing the last reading"
    )
    expect(status.className).toContain("text-amber-500")
  })

  it("refreshes when Retry is pressed", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      claude_error: "claude usage http 500",
      claude_stale: true,
      claude_error_kind: "other",
    })
    mount()
    await openPopover()
    expect(
      await screen.findByText("Couldn't refresh; showing the last reading")
    ).toBeVisible()
    expect(transportCalls("usage_meter")).toHaveLength(1)
    fireEvent.click(screen.getByRole("button", { name: "Retry" }))
    await waitFor(() => expect(transportCalls("usage_meter")).toHaveLength(2))
  })

  it("shows a red error only for an auth failure with no data", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      claude: null,
      claude_error: "claude access token absent",
      claude_stale: false,
      claude_error_kind: "auth",
    })
    mount()
    const trigger = await openPopover()
    expect(trigger).toHaveTextContent("4.6k today")
    const status = await screen.findByText(
      "Couldn't read Claude usage: claude access token absent"
    )
    expect(status.className).toContain("text-destructive")
    expect(screen.queryByText("5 h")).not.toBeInTheDocument()
    expect(screen.queryByText(/^Updated/)).not.toBeInTheDocument()
  })

  it("keeps a no-data rate limit amber", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      claude: null,
      claude_error: "claude usage http 429",
      claude_error_kind: "rate_limited",
      claude_retry_at: minutesFromNow(10),
    })
    mount()
    await openPopover()
    const status = await screen.findByText(
      /^Claude rate-limited the request; retrying in 10 min\.$/
    )
    expect(status.className).toContain("text-amber-500")
  })
})

describe("StatusBarUsage router profile switch", () => {
  /** The popover trigger, opened so the switch is on screen. */
  async function open() {
    mount()
    fireEvent.click(await screen.findByRole("button", { name: "Token usage" }))
    return screen.findByRole("radiogroup", { name: "Router" })
  }

  it("renders the stored profile as the checked option", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      router_profile: "equilibrado",
    })
    await open()
    const calidad = screen.getByRole("radio", { name: "Quality" })
    const equilibrado = screen.getByRole("radio", { name: "Balanced" })
    expect(calidad).toHaveAttribute("aria-checked", "false")
    expect(equilibrado).toHaveAttribute("aria-checked", "true")
    expect(
      screen.getByText("Prefers OpenCode's free models when quality is similar")
    ).toBeVisible()
  })

  it("falls back to calidad when the server sends no profile", async () => {
    // A server that predates the field must not leave the switch blank.
    mocks.transportCall.mockResolvedValue({
      ...meter,
      router_profile: undefined,
    })
    await open()
    expect(screen.getByRole("radio", { name: "Quality" })).toHaveAttribute(
      "aria-checked",
      "true"
    )
  })

  it("sends the other profile and reflects it once stored", async () => {
    // Stateful fake: the stored profile starts at `calidad` and the read
    // answers with whatever was written, so the refetch is meaningful.
    let stored = "calidad"
    mocks.transportCall.mockImplementation(async (name: string) => {
      if (name === "router_profile_set") {
        stored = "equilibrado"
        return { profile: stored }
      }
      return { ...meter, router_profile: stored }
    })
    await open()
    fireEvent.click(screen.getByRole("radio", { name: "Balanced" }))

    await waitFor(() =>
      expect(transportCalls("router_profile_set")).toHaveLength(1)
    )
    expect(transportCalls("router_profile_set")[0][1]).toEqual({
      profile: "equilibrado",
    })
    await waitFor(() =>
      expect(screen.getByRole("radio", { name: "Balanced" })).toHaveAttribute(
        "aria-checked",
        "true"
      )
    )
    // Refetched rather than assumed: the read carries the new value.
    expect(transportCalls("usage_meter").length).toBeGreaterThan(1)
  })

  it("restores the previous profile when the write is rejected", async () => {
    mocks.transportCall.mockImplementation(async (name: string) => {
      if (name === "router_profile_set") throw new Error("invalid_input")
      return meter
    })
    await open()
    fireEvent.click(screen.getByRole("radio", { name: "Balanced" }))

    await waitFor(() =>
      expect(transportCalls("router_profile_set")).toHaveLength(1)
    )
    await waitFor(() =>
      expect(screen.getByRole("radio", { name: "Quality" })).toHaveAttribute(
        "aria-checked",
        "true"
      )
    )
    // No refetch after a failed write.
    expect(transportCalls("usage_meter")).toHaveLength(1)
  })

  it("notes the automatic downgrade once the quota is nearly spent", async () => {
    mocks.transportCall.mockResolvedValue({
      ...meter,
      claude: {
        ...meter.claude!,
        seven_day: { percent: 93, resets_at: null },
      },
    })
    await open()
    expect(
      screen.getByText(
        "At 90% or more of the Claude quota, the router uses Balanced automatically."
      )
    ).toBeVisible()
  })

  it("does not show the automatic-downgrade note on a healthy quota", async () => {
    mocks.transportCall.mockResolvedValue(meter)
    await open()
    expect(
      screen.queryByText(/the router uses Balanced automatically/)
    ).not.toBeInTheDocument()
  })

  it("does not show the automatic-downgrade note once equilibrado is set", async () => {
    // The note is about `calidad` being overridden; already being on
    // `equilibrado` is the state it describes, not a change to announce.
    mocks.transportCall.mockResolvedValue({
      ...meter,
      router_profile: "equilibrado",
      claude: { ...meter.claude!, seven_day: { percent: 99, resets_at: null } },
    })
    await open()
    expect(
      screen.queryByText(/the router uses Balanced automatically/)
    ).not.toBeInTheDocument()
  })
})
