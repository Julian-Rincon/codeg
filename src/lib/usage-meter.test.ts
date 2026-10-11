import { beforeEach, describe, expect, it, vi } from "vitest"

import {
  asRouterProfile,
  ROUTER_PROFILES,
  scheduleQuotaResume,
  setRouterProfile,
} from "@/lib/usage-meter"

const mocks = vi.hoisted(() => ({ transportCall: vi.fn() }))

vi.mock("@/lib/transport", () => ({
  getTransport: () => ({ call: mocks.transportCall }),
}))

beforeEach(() => {
  mocks.transportCall.mockReset()
})

describe("asRouterProfile", () => {
  it("passes through the two known profiles", () => {
    expect(asRouterProfile("calidad")).toBe("calidad")
    expect(asRouterProfile("equilibrado")).toBe("equilibrado")
  })

  it("falls back to calidad for absent or unknown values", () => {
    // Matches the backend default, so a bad stored value can never leave the
    // switch showing a profile the router would not use.
    expect(asRouterProfile(undefined)).toBe("calidad")
    expect(asRouterProfile(null)).toBe("calidad")
    expect(asRouterProfile("")).toBe("calidad")
    expect(asRouterProfile("Quality")).toBe("calidad")
    expect(asRouterProfile("rapido")).toBe("calidad")
  })

  it("lists exactly the two profiles the switch renders", () => {
    expect([...ROUTER_PROFILES]).toEqual(["calidad", "equilibrado"])
  })
})

describe("setRouterProfile", () => {
  it("calls router_profile_set with the profile and returns the stored one", async () => {
    mocks.transportCall.mockResolvedValue({ profile: "equilibrado" })
    const result = await setRouterProfile("equilibrado")
    expect(mocks.transportCall).toHaveBeenCalledWith("router_profile_set", {
      profile: "equilibrado",
    })
    expect(result).toEqual({ profile: "equilibrado" })
  })

  it("rejects when the backend refuses the value", async () => {
    mocks.transportCall.mockRejectedValue(new Error("invalid_input"))
    await expect(setRouterProfile("calidad")).rejects.toThrow("invalid_input")
  })
})

describe("scheduleQuotaResume", () => {
  it("sends camelCase keys, the shape both backends deserialize", async () => {
    // The web handler is `#[serde(rename_all = "camelCase")]` and Tauri maps
    // command arguments from camelCase: snake_case keys were rejected with a
    // 422 ("missing field `connectionId`"), so "Esperar y continuar solo" never
    // saved a schedule.
    mocks.transportCall.mockResolvedValue({ id: "qr-1", resume_at: "2026-10-11T18:11:00Z" })
    const result = await scheduleQuotaResume({
      connection_id: "conn-1",
      conversation_id: 42,
      agent_type: "claude_code",
    })
    expect(mocks.transportCall).toHaveBeenCalledWith("quota_resume_schedule", {
      connectionId: "conn-1",
      conversationId: 42,
      agentType: "claude_code",
    })
    expect(result).toEqual({ id: "qr-1", resume_at: "2026-10-11T18:11:00Z" })
  })

  it("rejects when the backend refuses, so the card can show the error", async () => {
    mocks.transportCall.mockRejectedValue(new Error("422"))
    await expect(
      scheduleQuotaResume({ connection_id: "c", conversation_id: 1, agent_type: "claude_code" })
    ).rejects.toThrow("422")
  })
})
