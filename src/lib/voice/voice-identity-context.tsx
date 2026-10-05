"use client"

// Which voice a transcript speaks with: the agent of the conversation, plus the
// conversation id so phantom-voice can tell a conversation in the NEXUS folder
// apart. Provided once by the message list, read by every "listen" button.

import { createContext, useContext } from "react"

export interface VoiceIdentity {
  persona?: string | null
  conversationId?: number | null
}

const VoiceIdentityContext = createContext<VoiceIdentity>({})

export const VoiceIdentityProvider = VoiceIdentityContext.Provider

export function useVoiceIdentity(): VoiceIdentity {
  return useContext(VoiceIdentityContext)
}
