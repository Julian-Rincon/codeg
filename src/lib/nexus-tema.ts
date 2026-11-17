import { useEffect } from "react"

/** Pantallas que hoy muestran a NEXUS (centro de comando, modo voz). Mientras
 *  haya al menos una, <html> lleva `data-nexus-tema` y globals.css pinta la
 *  interfaz con los colores de NEXUS en vez del acento del modelo. */
let activas = 0

export function useNexusTema(activo = true): void {
  useEffect(() => {
    if (!activo) return
    const root = document.documentElement
    activas += 1
    root.setAttribute("data-nexus-tema", "")
    return () => {
      activas = Math.max(0, activas - 1)
      if (activas === 0) root.removeAttribute("data-nexus-tema")
    }
  }, [activo])
}
