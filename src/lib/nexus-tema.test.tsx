import { renderHook } from "@testing-library/react"
import { describe, expect, it } from "vitest"

import { useNexusTema } from "./nexus-tema"

describe("useNexusTema", () => {
  it("marca <html> mientras alguna pantalla de NEXUS está abierta", () => {
    const root = document.documentElement
    const a = renderHook(() => useNexusTema())
    const b = renderHook(() => useNexusTema())
    expect(root.hasAttribute("data-nexus-tema")).toBe(true)
    a.unmount()
    expect(root.hasAttribute("data-nexus-tema")).toBe(true)
    b.unmount()
    expect(root.hasAttribute("data-nexus-tema")).toBe(false)
  })

  it("no marca nada si no está activo", () => {
    const h = renderHook(() => useNexusTema(false))
    expect(document.documentElement.hasAttribute("data-nexus-tema")).toBe(false)
    h.unmount()
  })
})
