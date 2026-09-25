import { describe, expect, it } from "vitest"
import {
  DEFAULT_GALAXY_CONFIG,
  generateGalaxyParticles,
  mulberry32,
} from "./galaxy-particles"

describe("mulberry32", () => {
  it("is deterministic for a given seed", () => {
    const a = mulberry32(42)
    const b = mulberry32(42)
    const seqA = Array.from({ length: 20 }, () => a())
    const seqB = Array.from({ length: 20 }, () => b())
    expect(seqA).toEqual(seqB)
  })

  it("produces values in [0, 1)", () => {
    const rand = mulberry32(7)
    for (let i = 0; i < 200; i++) {
      const v = rand()
      expect(v).toBeGreaterThanOrEqual(0)
      expect(v).toBeLessThan(1)
    }
  })

  it("differs across seeds", () => {
    const a = mulberry32(1)()
    const b = mulberry32(2)()
    expect(a).not.toBe(b)
  })
})

describe("generateGalaxyParticles", () => {
  it("returns exactly `count` particles", () => {
    const particles = generateGalaxyParticles({
      ...DEFAULT_GALAXY_CONFIG,
      count: 123,
    })
    expect(particles).toHaveLength(123)
  })

  it("is deterministic for a fixed seed", () => {
    const a = generateGalaxyParticles({ ...DEFAULT_GALAXY_CONFIG, seed: 99 })
    const b = generateGalaxyParticles({ ...DEFAULT_GALAXY_CONFIG, seed: 99 })
    expect(a).toEqual(b)
  })

  it("differs when the seed changes", () => {
    const a = generateGalaxyParticles({ ...DEFAULT_GALAXY_CONFIG, seed: 1 })
    const b = generateGalaxyParticles({ ...DEFAULT_GALAXY_CONFIG, seed: 2 })
    expect(a).not.toEqual(b)
  })

  it("distributes particles evenly across arms via round-robin index", () => {
    const particles = generateGalaxyParticles({
      ...DEFAULT_GALAXY_CONFIG,
      count: 900,
      arms: 3,
    })
    const counts = [0, 0, 0]
    for (const p of particles) counts[p.arm]++
    expect(counts).toEqual([300, 300, 300])
  })

  it("keeps radius within [0, 1]", () => {
    const particles = generateGalaxyParticles(DEFAULT_GALAXY_CONFIG)
    for (const p of particles) {
      expect(p.radius).toBeGreaterThanOrEqual(0)
      expect(p.radius).toBeLessThanOrEqual(1)
    }
  })

  it("keeps brightness within (0, 1]", () => {
    const particles = generateGalaxyParticles(DEFAULT_GALAXY_CONFIG)
    for (const p of particles) {
      expect(p.brightness).toBeGreaterThan(0)
      expect(p.brightness).toBeLessThanOrEqual(1)
    }
  })

  it("biases radius toward the center (median well under 0.5)", () => {
    const particles = generateGalaxyParticles({
      ...DEFAULT_GALAXY_CONFIG,
      count: 2000,
    })
    const sorted = [...particles].map((p) => p.radius).sort((a, b) => a - b)
    const median = sorted[Math.floor(sorted.length / 2)]
    expect(median).toBeLessThan(0.5)
  })

  it("handles a single-arm config without dividing by zero", () => {
    const particles = generateGalaxyParticles({
      ...DEFAULT_GALAXY_CONFIG,
      arms: 1,
      count: 10,
    })
    expect(particles).toHaveLength(10)
    for (const p of particles) expect(p.arm).toBe(0)
  })
})
