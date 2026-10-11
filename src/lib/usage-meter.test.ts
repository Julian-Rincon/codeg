import { beforeEach, describe, expect, it, vi } from "vitest"

import {
  asRouterProfile,
  ROUTER_PROFILES,
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
