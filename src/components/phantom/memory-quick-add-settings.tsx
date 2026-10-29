"use client"

/**
 * Settings row for the composer's `#` memory quick-add (Claude Code sessions).
 * Device-local, like the other composer preferences; see `phantom-memory.ts`.
 */

import { useTranslations } from "next-intl"
import { BookMarked } from "lucide-react"

import { SettingsSection } from "@/components/shared/settings-section"
import { Switch } from "@/components/ui/switch"
import {
  setMemoryQuickAddEnabled,
  useMemoryQuickAddEnabled,
} from "@/lib/phantom-memory"

export function MemoryQuickAddSettingsSection() {
  const t = useTranslations("Phantom.memory.settings")
  const enabled = useMemoryQuickAddEnabled()
  return (
    <SettingsSection
      icon={BookMarked}
      title={t("title")}
      description={t("description")}
      htmlFor="phantom-memory-quick-add"
      control={
        <Switch
          id="phantom-memory-quick-add"
          checked={enabled}
          onCheckedChange={setMemoryQuickAddEnabled}
        />
      }
    />
  )
}
