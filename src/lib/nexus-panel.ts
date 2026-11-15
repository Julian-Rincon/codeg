"use client"

/**
 * NEXUS command center — the data shown on Phantom's home screen.
 *
 * `nexus panel` (NEXUS repo) writes the file; the backend (`commands::nexus_panel`)
 * hands it over untouched. Everything here is defensive: the panel is optional and
 * a missing or half-written one must leave the home screen as upstream ships it.
 */

import { getTransport } from "@/lib/transport"

export interface NexusPanelEvent {
  titulo: string
  inicio: string
  fin: string
  todo_el_dia: boolean
}

export interface NexusPanelTask {
  id: number
  texto: string
  vence: string | null
  vencida: boolean
}

export interface NexusPanel {
  generado: string
  fecha: string
  frase: string
  agenda_disponible: boolean
  hoy: NexusPanelEvent[]
  manana: NexusPanelEvent[]
  manana_fecha: string
  pendientes: NexusPanelTask[]
  aprendido: { id: number; texto: string }[]
  total_recuerdos: number
  estado: { nombre: string; ok: boolean }[]
}

/** A panel older than this is not shown: stale "today" is worse than none. */
export const PANEL_MAX_AGE_MS = 3 * 60 * 60 * 1000

function list<T>(value: unknown): T[] {
  return Array.isArray(value) ? (value as T[]) : []
}

/** Normalise whatever the file held into a panel, or `null` if it is unusable. */
export function parseNexusPanel(raw: unknown, now: number): NexusPanel | null {
  if (!raw || typeof raw !== "object") return null
  const r = raw as Record<string, unknown>
  if (typeof r.frase !== "string" || typeof r.generado !== "string") return null
  const at = Date.parse(r.generado)
  if (!Number.isFinite(at) || now - at > PANEL_MAX_AGE_MS) return null
  return {
    generado: r.generado,
    fecha: typeof r.fecha === "string" ? r.fecha : "",
    frase: r.frase,
    agenda_disponible: r.agenda_disponible !== false,
    hoy: list<NexusPanelEvent>(r.hoy),
    manana: list<NexusPanelEvent>(r.manana),
    manana_fecha: typeof r.manana_fecha === "string" ? r.manana_fecha : "",
    pendientes: list<NexusPanelTask>(r.pendientes),
    aprendido: list<{ id: number; texto: string }>(r.aprendido),
    total_recuerdos:
      typeof r.total_recuerdos === "number" ? r.total_recuerdos : 0,
    estado: list<{ nombre: string; ok: boolean }>(r.estado),
  }
}

export async function fetchNexusPanel(): Promise<NexusPanel | null> {
  const raw = await getTransport().call<unknown>("nexus_panel", {})
  return parseNexusPanel(raw, Date.now())
}

export interface RailMark {
  key: string
  label: string
  /** 0–100, position of the start along the rail. */
  left: number
  /** 0–100, length along the rail. */
  width: number
}

/**
 * The timed events between `now` and the end of tomorrow, as positions on a
 * rail that starts at `now`. Labels are dropped when they would collide.
 */
export function railMarks(
  panel: Pick<NexusPanel, "hoy" | "manana">,
  now: number
): { marks: RailMark[]; midnight: number } {
  const end = new Date(now)
  end.setHours(24, 0, 0, 0)
  const midnightAt = end.getTime()
  const railEnd = midnightAt + 24 * 60 * 60 * 1000
  const span = railEnd - now
  const pct = (t: number) =>
    ((Math.min(Math.max(t, now), railEnd) - now) / span) * 100
  const marks: RailMark[] = []
  let lastLabelAt = -6 // leaves room for the "Ahora" label at the origin
  for (const e of [...panel.hoy, ...panel.manana]) {
    if (e.todo_el_dia) continue
    const start = Date.parse(e.inicio)
    const finish = Date.parse(e.fin)
    if (!Number.isFinite(start) || !Number.isFinite(finish) || finish <= now)
      continue
    const left = pct(start)
    const d = new Date(start)
    const hhmm = `${d.getHours()}:${String(d.getMinutes()).padStart(2, "0")}`
    const fits = left - lastLabelAt >= 14 && left <= 84
    if (fits) lastLabelAt = left
    marks.push({
      key: `${e.inicio}-${e.titulo}`,
      label: fits ? `${hhmm} ${e.titulo}` : "",
      left,
      width: Math.max(pct(finish) - left, 0.8),
    })
  }
  return { marks, midnight: pct(midnightAt) }
}
