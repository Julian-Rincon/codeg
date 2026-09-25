// Energy-based voice activity detection over a stream of RMS frames sampled
// from a Web Audio `AnalyserNode`. Pure/functional on purpose: `stepVad` is a
// reducer (state, frame) -> (state, event), so `use-voice-live.ts` can drive
// it from a real mic and this module can be unit-tested with synthetic frame
// sequences, no DOM/AudioContext required.

export interface VadConfig {
  /** Multiplier applied to the adaptive noise floor to get the "speech
   *  started" threshold. */
  startThresholdMultiplier: number
  /** Absolute floor for the start threshold, so a near-silent room (noise
   *  floor ~0) doesn't trigger on a whisper of digital noise. */
  minStartThreshold: number
  /** How long RMS must stay below threshold before an utterance is
   *  considered finished (ms). */
  silenceHangoverMs: number
  /** Utterances shorter than this are rejected as noise blips, not sent to
   *  STT (ms). */
  minUtteranceMs: number
  /** EMA coefficient adapting the noise floor UP toward a louder ambient
   *  level while idle. */
  noiseFloorRelease: number
  /** EMA coefficient adapting the noise floor DOWN toward a quieter ambient
   *  level while idle (slower, so a brief loud sound doesn't need to "cool
   *  down" the floor as fast as it raised it). */
  noiseFloorAttack: number
}

export const DEFAULT_VAD_CONFIG: VadConfig = {
  startThresholdMultiplier: 2.5,
  minStartThreshold: 0.01,
  silenceHangoverMs: 700,
  minUtteranceMs: 300,
  noiseFloorRelease: 0.2,
  noiseFloorAttack: 0.05,
}

export type VadPhase = "idle" | "speaking"

export interface VadState {
  phase: VadPhase
  /** Adaptive ambient noise level, tracked continuously while `idle`. */
  noiseFloor: number
  /** Timestamp (ms) the current utterance started, or `null` when idle. */
  speechStartAt: number | null
  /** Timestamp (ms) of the most recent above-threshold frame while
   *  speaking. */
  lastAboveThresholdAt: number | null
}

export function createInitialVadState(): VadState {
  return {
    phase: "idle",
    noiseFloor: 0,
    speechStartAt: null,
    lastAboveThresholdAt: null,
  }
}

export type VadEvent =
  | { type: "speech-start" }
  | { type: "speech-end"; durationMs: number }
  /** Utterance ended before `minUtteranceMs` — discard the buffered audio,
   *  never send it to STT. */
  | { type: "speech-rejected"; durationMs: number }
  | null

export interface VadStepResult {
  state: VadState
  event: VadEvent
}

/** RMS (root-mean-square) energy of one audio frame. Accepts any indexable
 *  array of samples in [-1, 1], e.g. `Float32Array` from
 *  `AnalyserNode.getFloatTimeDomainData`. */
export function computeRms(frame: ArrayLike<number>): number {
  if (frame.length === 0) return 0
  let sum = 0
  for (let i = 0; i < frame.length; i++) {
    const v = frame[i]
    sum += v * v
  }
  return Math.sqrt(sum / frame.length)
}

/**
 * Advance the VAD by one frame. `timestampMs` must be monotonically
 * non-decreasing across calls (e.g. `performance.now()`).
 */
export function stepVad(
  state: VadState,
  rms: number,
  timestampMs: number,
  config: VadConfig = DEFAULT_VAD_CONFIG
): VadStepResult {
  const threshold = Math.max(
    config.minStartThreshold,
    state.noiseFloor * config.startThresholdMultiplier
  )
  const above = rms > threshold

  if (state.phase === "idle") {
    const alpha =
      rms > state.noiseFloor
        ? config.noiseFloorRelease
        : config.noiseFloorAttack
    const noiseFloor = state.noiseFloor + (rms - state.noiseFloor) * alpha

    if (above) {
      return {
        state: {
          phase: "speaking",
          noiseFloor,
          speechStartAt: timestampMs,
          lastAboveThresholdAt: timestampMs,
        },
        event: { type: "speech-start" },
      }
    }
    return {
      state: { ...state, noiseFloor },
      event: null,
    }
  }

  // phase === "speaking"
  if (above) {
    return {
      state: { ...state, lastAboveThresholdAt: timestampMs },
      event: null,
    }
  }

  const lastAbove = state.lastAboveThresholdAt ?? timestampMs
  const silenceMs = timestampMs - lastAbove
  if (silenceMs < config.silenceHangoverMs) {
    // Still within the hangover window — brief pause, not end of utterance.
    return { state, event: null }
  }

  const startAt = state.speechStartAt ?? timestampMs
  const durationMs = lastAbove - startAt
  const nextState: VadState = {
    ...createInitialVadState(),
    // Keep the learned ambient floor across utterances rather than resetting
    // to 0 (which would otherwise briefly over-trigger right after speech).
    noiseFloor: state.noiseFloor,
  }

  if (durationMs < config.minUtteranceMs) {
    return { state: nextState, event: { type: "speech-rejected", durationMs } }
  }
  return { state: nextState, event: { type: "speech-end", durationMs } }
}
