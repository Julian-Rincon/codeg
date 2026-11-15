"use client"

/**
 * NEXUS command center — Phantom's home screen, when NEXUS has something to say.
 *
 * Takes the place of the stock greeting: one sentence with the state of the day,
 * a rail of the next hours, and three blocks (pending, tomorrow, last learned).
 * It is built only from the theme tokens the stock home uses; the red "now" node
 * is NEXUS's own mark and the only fixed colour.
 *
 * Without a fresh panel it renders `fallback` (the stock greeting), so a machine
 * without NEXUS shows exactly what upstream ships. Copy is Spanish on purpose:
 * the panel's data is, and this keeps the fork out of the i18n message files.
 */

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react"
import { fetchNexusPanel, railMarks, type NexusPanel } from "@/lib/nexus-panel"

const REFRESH_MS = 60_000
const NEXUS_RED = "#ff2020"

function useNexusPanel(): { panel: NexusPanel | null; now: number } {
  const [state, setState] = useState<{ panel: NexusPanel | null; now: number }>(
    () => ({ panel: null, now: Date.now() })
  )
  useEffect(() => {
    let alive = true
    const load = () => {
      if (document.visibilityState === "hidden") return
      fetchNexusPanel()
        .then((panel) => alive && setState({ panel, now: Date.now() }))
        .catch(() => alive && setState({ panel: null, now: Date.now() }))
    }
    load()
    const timer = window.setInterval(load, REFRESH_MS)
    document.addEventListener("visibilitychange", load)
    return () => {
      alive = false
      window.clearInterval(timer)
      document.removeEventListener("visibilitychange", load)
    }
  }, [])
  return state
}

function dueLabel(vence: string | null, vencida: boolean): string {
  if (!vence) return "Sin fecha"
  const [, m, d] = vence.split("-").map(Number)
  const meses = [
    "ene",
    "feb",
    "mar",
    "abr",
    "may",
    "jun",
    "jul",
    "ago",
    "sep",
    "oct",
    "nov",
    "dic",
  ]
  const cuando = `${d} de ${meses[(m || 1) - 1]}`
  return vencida ? `Venció el ${cuando}` : `Vence el ${cuando}`
}

function hourLabel(iso: string): string {
  const d = new Date(iso)
  return `${d.getHours()}:${String(d.getMinutes()).padStart(2, "0")}`
}

function Block({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex min-w-0 flex-col gap-2 rounded-lg border border-border bg-card/50 px-3.5 py-3">
      <h2 className="text-xs font-semibold text-foreground">{title}</h2>
      {children}
    </section>
  )
}

function Empty({ children }: { children: ReactNode }) {
  return <p className="text-xs text-muted-foreground">{children}</p>
}

function Rail({ panel, now }: { panel: NexusPanel; now: number }) {
  const { marks, midnight } = useMemo(() => railMarks(panel, now), [panel, now])
  return (
    <div className="relative h-12" aria-hidden>
      <div className="absolute inset-x-0 top-3 h-px bg-border" />
      <div
        className="absolute top-1.5 h-3.5 w-px bg-muted-foreground/40"
        style={{ left: `${midnight}%` }}
      />
      {marks.map((m) => (
        <div
          key={m.key}
          className="absolute top-[0.5625rem] h-1.5 rounded-full bg-primary"
          style={{ left: `${m.left}%`, width: `${m.width}%` }}
        />
      ))}
      <div
        className="absolute left-0 top-1 size-4 rounded-full border"
        style={{ borderColor: NEXUS_RED }}
      />
      <div
        className="absolute left-[0.3125rem] top-[0.5625rem] size-1.5 rounded-full bg-foreground"
        style={{ boxShadow: `0 0 0.625rem 0.125rem ${NEXUS_RED}` }}
      />
      <span className="absolute left-0 top-7 text-[0.6875rem] text-foreground">
        Ahora
      </span>
      {marks
        .filter((m) => m.label)
        .map((m, i, all) => (
          <span
            key={m.key}
            className="absolute top-7 truncate text-[0.6875rem] text-muted-foreground"
            // Up to the next label (or the end of the rail), so two never overlap.
            style={{
              left: `${m.left}%`,
              maxWidth: `${(all[i + 1]?.left ?? 100) - m.left - 1.5}%`,
            }}
          >
            {m.label}
          </span>
        ))}
    </div>
  )
}

const rowButton =
  "-mx-1.5 flex min-w-0 cursor-pointer flex-col rounded-md px-1.5 py-0.5 text-left transition-colors hover:bg-accent/50 focus-visible:outline-2 focus-visible:outline-ring"

export function NexusCommandCenter({
  fallback,
  onAsk,
  onMain,
}: {
  fallback: ReactNode
  /** Puts a sentence in the composer (the user still presses send). */
  onAsk?: (text: string) => void
  /** Called while the home is NEXUS's own: the draft should use its agent. */
  onMain?: () => void
}) {
  const { panel, now } = useNexusPanel()
  // NEXUS runs on one agent, so the home hides the stock agent row (a sibling
  // that keys off these data attributes) until asked for.
  const [showAgents, setShowAgents] = useState(false)
  const onMainRef = useRef(onMain)
  useEffect(() => {
    onMainRef.current = onMain
  })
  const active = panel != null && !showAgents
  useEffect(() => {
    if (active) onMainRef.current?.()
  }, [active])
  if (!panel) return <>{fallback}</>

  // "Estado de la agenda. Estado de las tareas." — the second sentence takes
  // the same accent the stock greeting gives its highlighted words.
  const cut = panel.frase.indexOf(". ")
  const head = cut > 0 ? panel.frase.slice(0, cut + 1) : panel.frase
  const tail = cut > 0 ? panel.frase.slice(cut + 2) : ""
  const tomorrow = panel.manana.filter((e) => !e.todo_el_dia)
  const allDay = panel.manana.filter((e) => e.todo_el_dia)

  return (
    <div
      className="flex flex-col gap-4"
      data-nexus-command-center
      data-nexus-agents={showAgents ? "" : undefined}
    >
      <header className="flex flex-col gap-1.5 text-center">
        <p className="text-xs text-muted-foreground">{panel.fecha}</p>
        <h1 className="text-2xl font-semibold tracking-tight text-foreground sm:text-3xl">
          {head}
          {tail ? (
            <>
              {" "}
              <span className="bg-gradient-to-br from-primary via-primary/85 to-chart-3 bg-clip-text text-transparent">
                {tail}
              </span>
            </>
          ) : null}
        </h1>
      </header>

      {panel.agenda_disponible ? <Rail panel={panel} now={now} /> : null}

      <div className="grid grid-cols-1 gap-2 sm:grid-cols-3">
        <Block title="Pendiente">
          {panel.pendientes.length === 0 ? (
            <Empty>Nada pendiente. Dime una tarea y la anoto.</Empty>
          ) : (
            panel.pendientes.slice(0, 3).map((t) => (
              <button
                key={t.id}
                type="button"
                className={rowButton}
                title="Pedirle ayuda a NEXUS con esta tarea"
                onClick={() =>
                  onAsk?.(`Ayúdame con la tarea #${t.id}: ${t.texto}`)
                }
              >
                <span className="line-clamp-2 text-xs text-foreground/90">
                  {t.texto}
                </span>
                <span
                  className={
                    t.vencida
                      ? "text-[0.6875rem] text-amber-600 dark:text-amber-400"
                      : "text-[0.6875rem] text-muted-foreground"
                  }
                >
                  {dueLabel(t.vence, t.vencida)}
                </span>
              </button>
            ))
          )}
          {panel.pendientes.length > 3 ? (
            <Empty>y {panel.pendientes.length - 3} más</Empty>
          ) : null}
        </Block>

        <Block title={`Mañana, ${panel.manana_fecha}`}>
          {!panel.agenda_disponible ? (
            <Empty>
              No pude leer el calendario. Reintento en unos minutos.
            </Empty>
          ) : tomorrow.length === 0 && allDay.length === 0 ? (
            <Empty>Nada en la agenda.</Empty>
          ) : (
            <>
              {tomorrow.slice(0, 3).map((e) => (
                <div
                  key={`${e.inicio}-${e.titulo}`}
                  className="flex min-w-0 gap-2 text-xs"
                >
                  <span className="w-9 shrink-0 tabular-nums text-muted-foreground">
                    {hourLabel(e.inicio)}
                  </span>
                  <span className="line-clamp-2 text-foreground/90">
                    {e.titulo}
                  </span>
                </div>
              ))}
              {allDay.slice(0, 2).map((e) => (
                <div
                  key={`${e.inicio}-${e.titulo}`}
                  className="flex min-w-0 gap-2 text-xs"
                >
                  <span className="w-9 shrink-0 text-muted-foreground">
                    Día
                  </span>
                  <span className="line-clamp-1 text-foreground/90">
                    {e.titulo}
                  </span>
                </div>
              ))}
            </>
          )}
        </Block>

        <Block title="Lo último que aprendí de ti">
          {panel.aprendido.length === 0 ? (
            <Empty>Todavía nada. Lo que me cuentes lo voy guardando.</Empty>
          ) : (
            panel.aprendido.slice(0, 2).map((r) => (
              <span
                key={r.id}
                className="line-clamp-2 text-xs text-foreground/90"
              >
                {r.texto}
              </span>
            ))
          )}
          {panel.total_recuerdos > 0 ? (
            <button
              type="button"
              className={`${rowButton} text-xs text-muted-foreground`}
              onClick={() =>
                onAsk?.(
                  "Muéstrame lo que has aprendido de mí esta semana, para corregirte lo que esté mal."
                )
              }
            >
              {panel.total_recuerdos} recuerdos. Repasarlos
            </button>
          ) : null}
        </Block>
      </div>

      {panel.estado.length > 0 ? (
        <ul className="flex flex-wrap justify-center gap-x-5 gap-y-1 text-[0.6875rem] text-muted-foreground">
          {panel.estado.map((s) => (
            <li key={s.nombre} className="flex items-center gap-1.5">
              <span
                aria-hidden
                className={
                  s.ok
                    ? "size-1.5 rounded-full bg-foreground/70"
                    : "size-1.5 rounded-full border border-amber-500"
                }
              />
              {s.ok ? s.nombre : `${s.nombre}: revisar`}
            </li>
          ))}
        </ul>
      ) : null}

      <button
        type="button"
        onClick={() => setShowAgents((v) => !v)}
        aria-expanded={showAgents}
        className="mx-auto rounded-md px-2 py-1 text-[0.6875rem] text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
      >
        {showAgents ? "Volver a solo NEXUS" : "Usar otro agente esta vez"}
      </button>
    </div>
  )
}
