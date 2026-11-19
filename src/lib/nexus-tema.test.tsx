import { renderHook } from "@testing-library/react"
import { describe, expect, it, vi } from "vitest"

import { migrarEstiloNexus, useNexusTema } from "./nexus-tema"

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

describe("migrarEstiloNexus", () => {
  function almacen() {
    const datos = new Map<string, string>()
    return {
      getItem: (k: string) => datos.get(k) ?? null,
      setItem: (k: string, v: string) => void datos.set(k, v),
    }
  }

  it("pasa el rojo genérico a NEXUS una sola vez", () => {
    const a = almacen()
    const cambiar = vi.fn()
    expect(migrarEstiloNexus("red", cambiar, a)).toBe(true)
    expect(cambiar).toHaveBeenCalledWith("nexus")
    expect(migrarEstiloNexus("red", cambiar, a)).toBe(false)
    expect(cambiar).toHaveBeenCalledTimes(1)
  })

  it("respeta cualquier otro estilo elegido", () => {
    const cambiar = vi.fn()
    expect(migrarEstiloNexus("violet", cambiar, almacen())).toBe(false)
    expect(cambiar).not.toHaveBeenCalled()
  })

  it("sin almacenamiento no hace nada", () => {
    const cambiar = vi.fn()
    expect(migrarEstiloNexus("red", cambiar, null)).toBe(false)
    expect(cambiar).not.toHaveBeenCalled()
  })
})
