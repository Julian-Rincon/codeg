// Pure, seeded particle-field generator for the voice-live galaxy orb
// (`voice-galaxy-orb.tsx`). Kept dependency-free from Canvas/DOM so it can be
// unit-tested for determinism (same seed -> identical output) and particle
// count without a browser environment.

/** Deterministic PRNG (mulberry32) — fast, good-enough distribution for a
 *  decorative particle field, and critically: reproducible across runs given
 *  the same seed, which `generateGalaxyParticles` relies on for testability. */
export function mulberry32(seed: number): () => number {
  let a = seed >>> 0
  return function next() {
    a |= 0
    a = (a + 0x6d2b79f5) | 0
    let t = Math.imul(a ^ (a >>> 15), 1 | a)
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296
  }
}

export interface GalaxyParticle {
  /** Base angle (radians) around the center, BEFORE any per-frame rotation or
   *  spiral-winding offset is applied — the orb component adds those at draw
   *  time so the particle field itself never needs to be regenerated. */
  angle: number
  /** Normalized distance from the center, 0 (core) .. 1 (edge). */
  radius: number
  /** Which spiral arm this particle belongs to (0-indexed). */
  arm: number
  /** Relative point size, roughly 0.3 (dust) .. 1.4 (bright core star). */
  size: number
  /** Base brightness 0..1 before twinkle modulation; higher near the core. */
  brightness: number
  /** Phase offset (radians) for the per-particle twinkle oscillation. */
  twinklePhase: number
  /** Per-particle twinkle speed multiplier, so stars don't blink in unison. */
  twinkleSpeed: number
}

export interface GalaxyConfig {
  /** Total particle count. A few hundred to ~1500 keeps 60fps on modest GPUs. */
  count: number
  /** Number of spiral arms (2-4 reads as a galaxy; 1 or 5+ looks wrong). */
  arms: number
  /** Angular jitter around each arm's ideal spiral curve, in radians. */
  armSpread: number
  /** How many radians the arm winds per unit of normalized radius. */
  spiralTightness: number
  /** PRNG seed — same seed always yields the same field. */
  seed: number
}

export const DEFAULT_GALAXY_CONFIG: GalaxyConfig = {
  count: 800,
  arms: 3,
  armSpread: 0.55,
  spiralTightness: 3.4,
  seed: 1337,
}

/**
 * Build a static spiral-galaxy particle field. Radius is biased toward the
 * center (t^1.6) so the core reads as dense and bright while the arms thin
 * out toward the edge, matching a real spiral galaxy's light profile.
 */
export function generateGalaxyParticles(
  config: GalaxyConfig = DEFAULT_GALAXY_CONFIG
): GalaxyParticle[] {
  const rand = mulberry32(config.seed)
  const particles: GalaxyParticle[] = []
  const arms = Math.max(1, Math.floor(config.arms))

  for (let i = 0; i < config.count; i++) {
    const arm = i % arms
    const t = rand()
    const radius = Math.pow(t, 1.6)
    const armBaseAngle = (arm / arms) * Math.PI * 2
    const spiralAngle = radius * config.spiralTightness
    // Jitter grows with radius: the core stays tight, the outer arms fray.
    const scatter = (rand() - 0.5) * config.armSpread * (0.3 + radius)
    const angle = armBaseAngle + spiralAngle + scatter
    const brightness = Math.max(0.15, 1 - radius) * (0.6 + rand() * 0.4)
    const size = (0.4 + rand() * 1.0) * (1 - radius * 0.5)

    particles.push({
      angle,
      radius,
      arm,
      size,
      brightness,
      twinklePhase: rand() * Math.PI * 2,
      twinkleSpeed: 0.5 + rand() * 1.5,
    })
  }

  return particles
}
