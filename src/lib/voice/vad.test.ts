import { describe, expect, it } from "vitest"
import {
  computeRms,
  createInitialVadState,
  DEFAULT_VAD_CONFIG,
  stepVad,
  type VadConfig,
  type VadState,
} from "./vad"

describe("computeRms", () => {
  it("is 0 for silence", () => {
    expect(computeRms(new Float32Array(100))).toBe(0)
  })

  it("is 0 for an empty frame", () => {
    expect(computeRms(new Float32Array(0))).toBe(0)
  })

  it("computes RMS for a constant-amplitude frame", () => {
    const frame = new Float32Array(10).fill(0.5)
    expect(computeRms(frame)).toBeCloseTo(0.5, 5)
  })

  it("computes RMS for a mixed frame", () => {
    // RMS([1, -1, 1, -1]) === 1
    const frame = new Float32Array([1, -1, 1, -1])
    expect(computeRms(frame)).toBeCloseTo(1, 5)
  })
})

// Feed a sequence of synthetic (rms, dt) frames through the reducer and
// collect the events, mirroring how `use-voice-live.ts` drives it from a
// real AnalyserNode poll loop.
function runFrames(
  frames: Array<{ rms: number; dt: number }>,
  config: VadConfig = DEFAULT_VAD_CONFIG
): { events: ReturnType<typeof stepVad>["event"][]; finalState: VadState } {
  let state = createInitialVadState()
  let t = 0
  const events: ReturnType<typeof stepVad>["event"][] = []
  for (const frame of frames) {
    t += frame.dt
    const result = stepVad(state, frame.rms, t, config)
    state = result.state
    events.push(result.event)
  }
  return { events, finalState: state }
}

const SILENT = 0.0005
const LOUD = 0.5

describe("stepVad", () => {
  it("stays idle on continuous silence", () => {
    const frames = Array.from({ length: 20 }, () => ({ rms: SILENT, dt: 50 }))
    const { events, finalState } = runFrames(frames)
    expect(events.every((e) => e === null)).toBe(true)
    expect(finalState.phase).toBe("idle")
  })

  it("emits speech-start as soon as RMS crosses the threshold", () => {
    const frames = [
      { rms: SILENT, dt: 50 },
      { rms: SILENT, dt: 50 },
      { rms: LOUD, dt: 50 },
    ]
    const { events } = runFrames(frames)
    expect(events[2]).toEqual({ type: "speech-start" })
  })

  it("emits speech-end after silenceHangoverMs of trailing silence, for an utterance at least minUtteranceMs long", () => {
    const frames = [
      { rms: SILENT, dt: 50 }, // warm up noise floor
      { rms: SILENT, dt: 50 },
      { rms: LOUD, dt: 50 }, // speech-start, t=150
      { rms: LOUD, dt: 400 }, // still speaking, t=550 (400ms utterance so far)
      // Silence begins at t=550. Hangover is 700ms, so speech-end should not
      // fire until t >= 1250.
      { rms: SILENT, dt: 300 }, // t=850, 300ms silence — still within hangover
      { rms: SILENT, dt: 300 }, // t=1150, 600ms silence — still within hangover
      { rms: SILENT, dt: 200 }, // t=1350, 800ms silence — hangover elapsed
    ]
    const { events } = runFrames(frames)
    const endIdx = events.findIndex((e) => e?.type === "speech-end")
    expect(endIdx).toBe(6)
    const endEvent = events[endIdx]
    expect(endEvent?.type).toBe("speech-end")
    if (endEvent?.type === "speech-end") {
      // Duration measured from speech-start to the last above-threshold frame
      // (t=550), i.e. 400ms — not including the trailing silence itself.
      expect(endEvent.durationMs).toBe(400)
    }
  })

  it("rejects an utterance shorter than minUtteranceMs as a noise blip", () => {
    const frames = [
      { rms: SILENT, dt: 50 },
      { rms: SILENT, dt: 50 },
      { rms: LOUD, dt: 50 }, // speech-start, t=150
      { rms: LOUD, dt: 100 }, // t=250, 100ms utterance — below minUtteranceMs (300)
      { rms: SILENT, dt: 700 }, // t=950, 700ms silence — hangover elapsed
      { rms: SILENT, dt: 50 },
    ]
    const { events } = runFrames(frames)
    const rejected = events.find((e) => e?.type === "speech-rejected")
    expect(rejected).toBeTruthy()
    if (rejected?.type === "speech-rejected") {
      expect(rejected.durationMs).toBe(100)
    }
    expect(events.some((e) => e?.type === "speech-end")).toBe(false)
  })

  it("does not end the utterance on a brief dip under the hangover window", () => {
    const frames = [
      { rms: SILENT, dt: 50 },
      { rms: LOUD, dt: 50 }, // speech-start
      { rms: LOUD, dt: 400 },
      { rms: SILENT, dt: 200 }, // brief dip, well under 700ms hangover
      { rms: LOUD, dt: 50 }, // speech resumes — no speech-end should have fired
    ]
    const { events } = runFrames(frames)
    expect(events.some((e) => e?.type === "speech-end")).toBe(false)
    expect(events.some((e) => e?.type === "speech-rejected")).toBe(false)
  })

  it("adapts the noise floor upward in a louder room, raising the effective threshold", () => {
    const quietFrames = Array.from({ length: 30 }, () => ({
      rms: 0.01,
      dt: 50,
    }))
    const { finalState: quietState } = runFrames(quietFrames)

    const loudRoomFrames = Array.from({ length: 30 }, () => ({
      rms: 0.05,
      dt: 50,
    }))
    const { finalState: loudRoomState } = runFrames(loudRoomFrames)

    expect(loudRoomState.noiseFloor).toBeGreaterThan(quietState.noiseFloor)
  })

  it("never emits an event while below the minStartThreshold floor, even with a near-zero noise floor", () => {
    const frames = Array.from({ length: 10 }, () => ({
      rms: DEFAULT_VAD_CONFIG.minStartThreshold * 0.5,
      dt: 50,
    }))
    const { events } = runFrames(frames)
    expect(events.every((e) => e === null)).toBe(true)
  })

  it("resets speechStartAt/lastAboveThresholdAt to null after an utterance ends", () => {
    const frames = [
      { rms: SILENT, dt: 50 },
      { rms: LOUD, dt: 50 },
      { rms: LOUD, dt: 400 },
      { rms: SILENT, dt: 800 },
    ]
    const { finalState } = runFrames(frames)
    expect(finalState.phase).toBe("idle")
    expect(finalState.speechStartAt).toBeNull()
    expect(finalState.lastAboveThresholdAt).toBeNull()
  })
})
