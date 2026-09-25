"use client"

// The voice-live overlay's central visual: a rotating HUD ring (Phantom logo
// motif) wrapped around a spinning spiral galaxy. Canvas 2D on purpose — at
// ~800 particles it holds 60fps on modest hardware without WebGL's setup
// cost, and DPR is capped at 2 to keep the backing store bounded on hi-DPI
// displays. All motion pauses when the tab is hidden (`visibilitychange`)
// and collapses to a single static frame + CSS opacity pulse under
// `prefers-reduced-motion`.

import { useEffect, useRef, type RefObject } from "react"
import { useMediaQuery } from "@/hooks/use-media-query"
import { cn } from "@/lib/utils"
import {
  DEFAULT_GALAXY_CONFIG,
  generateGalaxyParticles,
} from "@/lib/voice/galaxy-particles"

export type VoiceOrbState = "idle" | "listening" | "thinking" | "speaking"

export interface VoiceGalaxyOrbProps {
  state: VoiceOrbState
  /** Muted mic dims the orb regardless of `state` (spec: "idle/muted -> slow,
   *  dimmer"). */
  muted?: boolean
  /** 0..1, read every frame directly — see `use-voice-live.ts`. */
  micLevelRef: RefObject<number>
  /** 0..1, read every frame directly. */
  ttsLevelRef: RefObject<number>
  size?: number
  className?: string
}

interface RgbColor {
  r: number
  g: number
  b: number
}

const FALLBACK_ACCENT: RgbColor = { r: 59, g: 130, b: 246 } // phantom general-blue
const NAVY_BG: RgbColor = { r: 7, g: 11, b: 24 }

function hexToRgb(hex: string): RgbColor {
  const clean = hex.replace("#", "").trim()
  if (!/^[0-9a-fA-F]{3}([0-9a-fA-F]{3})?$/.test(clean)) return FALLBACK_ACCENT
  const full =
    clean.length === 3
      ? clean
          .split("")
          .map((c) => c + c)
          .join("")
      : clean
  const num = parseInt(full, 16)
  return { r: (num >> 16) & 255, g: (num >> 8) & 255, b: num & 255 }
}

function mixWithWhite(c: RgbColor, amount: number): RgbColor {
  const t = Math.min(1, Math.max(0, amount))
  return {
    r: c.r + (255 - c.r) * t,
    g: c.g + (255 - c.g) * t,
    b: c.b + (255 - c.b) * t,
  }
}

function rgba(c: RgbColor, alpha: number): string {
  return `rgba(${c.r | 0}, ${c.g | 0}, ${c.b | 0}, ${Math.max(0, Math.min(1, alpha))})`
}

function readAccentColor(): RgbColor {
  if (typeof window === "undefined" || typeof document === "undefined") {
    return FALLBACK_ACCENT
  }
  const value = getComputedStyle(document.documentElement)
    .getPropertyValue("--phantom-accent")
    .trim()
  return value ? hexToRgb(value) : FALLBACK_ACCENT
}

// Generated once at module scope: the particle field is a fixed, seeded
// shape — only its color, rotation and pulse are animated per frame.
const GALAXY_PARTICLES = generateGalaxyParticles(DEFAULT_GALAXY_CONFIG)

const RING_COUNT = 3
// Radians/sec base speed per ring, alternating direction so the rings read
// as independent layers rather than one rigid disc.
const RING_BASE_SPEED = [0.28, -0.18, 0.11]
const RING_TICK_COUNT = [22, 16, 30]

export function VoiceGalaxyOrb({
  state,
  muted = false,
  micLevelRef,
  ttsLevelRef,
  size = 220,
  className,
}: VoiceGalaxyOrbProps) {
  const canvasRef = useRef<HTMLCanvasElement | null>(null)
  const stateRef = useRef(state)
  const mutedRef = useRef(muted)
  const accentRef = useRef<RgbColor>(FALLBACK_ACCENT)
  const reducedMotion = useMediaQuery("(prefers-reduced-motion: reduce)")

  // Refs, not state: `draw`'s rAF loop reads these every frame and must
  // never itself trigger a re-render. Written from an effect (not during
  // render) per the rules of hooks.
  useEffect(() => {
    stateRef.current = state
  }, [state])
  useEffect(() => {
    mutedRef.current = muted
  }, [muted])

  // Track the active model accent (`--phantom-accent`), re-reading it
  // whenever the app switches model/theme.
  useEffect(() => {
    accentRef.current = readAccentColor()
    const observer = new MutationObserver(() => {
      accentRef.current = readAccentColor()
    })
    observer.observe(document.documentElement, {
      attributes: true,
      attributeFilter: ["data-phantom-accent", "class"],
    })
    return () => observer.disconnect()
  }, [])

  useEffect(() => {
    const canvas = canvasRef.current
    if (!canvas) return
    const maybeCtx = canvas.getContext("2d")
    if (!maybeCtx) return // e.g. no canvas backend in a headless test environment
    // Give `ctx` a genuinely non-nullable TYPE (not just a narrowed one) so it
    // stays usable inside `draw`, a function declaration whose body TS
    // otherwise re-widens back to `CanvasRenderingContext2D | null`.
    const ctx: CanvasRenderingContext2D = maybeCtx

    const dpr = Math.min(
      typeof window !== "undefined" ? window.devicePixelRatio || 1 : 1,
      2
    )
    canvas.width = size * dpr
    canvas.height = size * dpr
    canvas.style.width = `${size}px`
    canvas.style.height = `${size}px`
    ctx.scale(dpr, dpr)

    const cx = size / 2
    const cy = size / 2
    const galaxyRadius = size * 0.34
    const ringBaseRadius = size * 0.46

    let rafId: number | null = null
    let animating = true
    let galaxyRotation = 0
    let thinkingSwirl = 0
    let lastTs: number | null = null
    const ringRotations = new Array(RING_COUNT).fill(0) as number[]

    function speedMultiplier(): number {
      if (mutedRef.current || stateRef.current === "idle") return 0.5
      if (stateRef.current === "thinking") return 1.8
      return 1
    }

    function draw(ts: number) {
      if (lastTs == null) lastTs = ts
      const dt = Math.min(0.05, (ts - lastTs) / 1000)
      lastTs = ts

      const s = stateRef.current
      const mic = micLevelRef.current ?? 0
      const tts = ttsLevelRef.current ?? 0
      const dim = mutedRef.current || s === "idle" ? 0.55 : 1
      const mult = speedMultiplier()

      galaxyRotation +=
        dt * 0.18 * mult * (s === "listening" ? 1 + mic * 0.8 : 1)
      thinkingSwirl =
        s === "thinking"
          ? Math.min(thinkingSwirl + dt * 0.5, 2.5)
          : Math.max(thinkingSwirl - dt * 1.2, 0)
      for (let i = 0; i < RING_COUNT; i++) {
        ringRotations[i] += dt * RING_BASE_SPEED[i] * mult
      }

      const accent = accentRef.current
      const core = mixWithWhite(accent, 0.55)

      ctx.clearRect(0, 0, size, size)
      ctx.fillStyle = rgba(NAVY_BG, 1)
      ctx.beginPath()
      ctx.arc(cx, cy, size / 2, 0, Math.PI * 2)
      ctx.fill()

      // HUD rings: dashed arcs + tick marks, each rotating independently.
      for (let i = 0; i < RING_COUNT; i++) {
        const r = ringBaseRadius - i * size * 0.045
        ctx.save()
        ctx.translate(cx, cy)
        ctx.rotate(ringRotations[i])
        ctx.lineWidth = 1.2
        ctx.setLineDash([r * 0.18, r * 0.14])
        ctx.strokeStyle = rgba(
          accent,
          (0.3 + (s === "speaking" ? tts * 0.4 : 0)) * dim
        )
        ctx.beginPath()
        ctx.arc(0, 0, r, 0, Math.PI * 2)
        ctx.stroke()

        ctx.setLineDash([])
        ctx.strokeStyle = rgba(accent, 0.5 * dim)
        const ticks = RING_TICK_COUNT[i]
        for (let t = 0; t < ticks; t++) {
          const a = (t / ticks) * Math.PI * 2
          ctx.beginPath()
          ctx.moveTo(Math.cos(a) * (r - 3), Math.sin(a) * (r - 3))
          ctx.lineTo(Math.cos(a) * (r + 3), Math.sin(a) * (r + 3))
          ctx.stroke()
        }
        ctx.restore()
      }

      // Spiral galaxy: log-spiral arms, drawn as tiny fills (cheap vs. one
      // arc() call per star).
      ctx.save()
      ctx.translate(cx, cy)
      const pulseScale =
        1 + (s === "listening" ? mic * 0.15 : s === "speaking" ? tts * 0.1 : 0)
      for (const p of GALAXY_PARTICLES) {
        const angle = p.angle + galaxyRotation + p.radius * thinkingSwirl
        const r = p.radius * galaxyRadius * pulseScale
        const x = Math.cos(angle) * r
        // Slight ellipse: reads as a galaxy tilted toward the viewer.
        const y = Math.sin(angle) * r * 0.6
        const twinkle =
          0.7 + 0.3 * Math.sin(ts * 0.001 * p.twinkleSpeed + p.twinklePhase)
        const brightness = Math.min(1, p.brightness * twinkle * dim)
        const color = mixWithWhite(accent, Math.min(0.75, (1 - p.radius) * 0.6))
        ctx.fillStyle = rgba(color, brightness)
        const px = Math.max(0.6, p.size * 1.6)
        ctx.fillRect(x - px / 2, y - px / 2, px, px)
      }
      ctx.restore()

      // Core glow, pulsing with mic (listening) or TTS (speaking) level.
      const coreRadius =
        size *
        0.09 *
        (1 +
          (s === "speaking" ? tts * 0.5 : s === "listening" ? mic * 0.25 : 0))
      const gradient = ctx.createRadialGradient(
        cx,
        cy,
        0,
        cx,
        cy,
        coreRadius * 3
      )
      gradient.addColorStop(0, rgba(core, 0.9 * dim))
      gradient.addColorStop(0.4, rgba(accent, 0.35 * dim))
      gradient.addColorStop(1, rgba(accent, 0))
      ctx.fillStyle = gradient
      ctx.beginPath()
      ctx.arc(cx, cy, coreRadius * 3, 0, Math.PI * 2)
      ctx.fill()
      ctx.fillStyle = rgba(core, dim)
      ctx.beginPath()
      ctx.arc(cx, cy, coreRadius, 0, Math.PI * 2)
      ctx.fill()

      if (animating) rafId = requestAnimationFrame(draw)
    }

    function handleVisibility() {
      if (document.hidden) {
        animating = false
        if (rafId != null) cancelAnimationFrame(rafId)
        rafId = null
      } else if (!reducedMotion) {
        animating = true
        lastTs = null
        rafId = requestAnimationFrame(draw)
      }
    }

    if (reducedMotion) {
      animating = false
      draw(0) // one static frame; CSS handles the gentle opacity pulse
    } else {
      rafId = requestAnimationFrame(draw)
    }
    document.addEventListener("visibilitychange", handleVisibility)

    return () => {
      animating = false
      document.removeEventListener("visibilitychange", handleVisibility)
      if (rafId != null) cancelAnimationFrame(rafId)
    }
  }, [size, reducedMotion, micLevelRef, ttsLevelRef])

  return (
    <canvas
      ref={canvasRef}
      aria-hidden="true"
      className={cn(
        "rounded-full",
        reducedMotion && "animate-pulse",
        className
      )}
    />
  )
}
