import { useContext, useEffect } from "react"
import { AppearanceContext } from "@/components/appearance-provider"

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

const MARCA_MIGRACION = "phantom-estilo-nexus-migrado"

/** Una sola vez por equipo: quien había elegido el estilo rojo genérico para
 *  "ver a NEXUS" pasa al estilo NEXUS propio. Después se respeta lo que elija. */
export function migrarEstiloNexus(
  actual: string,
  cambiar: (color: "nexus") => void,
  almacen: Pick<Storage, "getItem" | "setItem"> | null = typeof window ===
  "undefined"
    ? null
    : window.localStorage
): boolean {
  if (!almacen) return false
  try {
    if (almacen.getItem(MARCA_MIGRACION)) return false
    almacen.setItem(MARCA_MIGRACION, "1")
  } catch {
    return false
  }
  if (actual !== "red") return false
  cambiar("nexus")
  return true
}

export function useMigrarEstiloNexus(): void {
  const apariencia = useContext(AppearanceContext)
  const actual = apariencia?.themeColor
  const cambiar = apariencia?.setThemeColor
  useEffect(() => {
    if (!actual || !cambiar) return
    migrarEstiloNexus(actual, cambiar)
    // Solo al montar: la marca evita repetirlo.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])
}
