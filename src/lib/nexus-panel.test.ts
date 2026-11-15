import { describe, expect, it } from "vitest"
import { parseNexusPanel, railMarks, PANEL_MAX_AGE_MS } from "./nexus-panel"

const NOW = new Date(2026, 10, 15, 12, 0, 0).getTime()
const iso = (day: number, h: number) =>
  new Date(2026, 10, day, h, 0, 0).toISOString()

describe("parseNexusPanel", () => {
  it("accepts a fresh panel and fills what is missing", () => {
    const p = parseNexusPanel(
      { frase: "Hola.", generado: new Date(NOW).toISOString() },
      NOW
    )
    expect(p?.frase).toBe("Hola.")
    expect(p?.pendientes).toEqual([])
    expect(p?.agenda_disponible).toBe(true)
  })

  it("refuses a stale, shapeless or missing panel", () => {
    const old = new Date(NOW - PANEL_MAX_AGE_MS - 1000).toISOString()
    expect(parseNexusPanel({ frase: "x", generado: old }, NOW)).toBeNull()
    expect(
      parseNexusPanel({ generado: new Date(NOW).toISOString() }, NOW)
    ).toBeNull()
    expect(parseNexusPanel(null, NOW)).toBeNull()
    expect(parseNexusPanel([1], NOW)).toBeNull()
  })
})

describe("railMarks", () => {
  const ev = (
    day: number,
    h1: number,
    h2: number,
    titulo: string,
    todo = false
  ) => ({
    titulo,
    inicio: iso(day, h1),
    fin: iso(day, h2),
    todo_el_dia: todo,
  })

  it("places what is left of today and tomorrow, skipping past and all-day events", () => {
    const { marks, midnight } = railMarks(
      {
        hoy: [ev(15, 8, 9, "Pasó"), ev(15, 18, 20, "Clase")],
        manana: [ev(16, 0, 24, "Festivo", true), ev(16, 9, 11, "Gym")],
      },
      NOW
    )
    expect(midnight).toBeCloseTo((12 / 36) * 100)
    expect(marks.map((m) => m.label)).toEqual(["18:00 Clase", "9:00 Gym"])
    expect(marks[0].left).toBeCloseTo((6 / 36) * 100)
    expect(marks[1].width).toBeCloseTo((2 / 36) * 100)
  })

  it("drops the label of an event that would collide with the previous one", () => {
    const { marks } = railMarks(
      { hoy: [ev(15, 18, 19, "A"), ev(15, 19, 20, "B")], manana: [] },
      NOW
    )
    expect(marks.map((m) => m.label)).toEqual(["18:00 A", ""])
  })
})
