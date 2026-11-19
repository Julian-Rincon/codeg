#!/usr/bin/env bash
# Verifica que los puntos de enganche de NEXUS/Phantom sobrevivieron a un merge
# de upstream. Uso: phantom-verificar-fork.sh [ruta-del-checkout-de-codeg]
# Sale con 0 si todo está; con 1 y la lista de lo que falta si no.
set -u
raiz="${1:-.}"
falta=0
revisar() { # archivo, patrón fijo, veces mínimas
  local n
  n=$(grep -cF -- "$2" "$raiz/$1" 2>/dev/null || true)
  if [ "${n:-0}" -lt "${3:-1}" ]; then
    echo "FALTA  $1  ←  $2  (hay ${n:-0}, se esperan ${3:-1})"
    falta=1
  fi
}
existe() { [ -e "$raiz/$1" ] || { echo "FALTA  archivo $1"; falta=1; }; }

existe src-tauri/src/chat_channel/nexus_chat.rs
existe src-tauri/src/phantom/worktrees.rs
existe src-tauri/src/commands/nexus_panel.rs
existe src-tauri/src/web/handlers/nexus_panel.rs
existe src/lib/nexus-panel.ts
existe src/components/phantom/nexus-command-center.tsx

revisar src-tauri/src/chat_channel/mod.rs "pub mod nexus_chat;"
revisar src-tauri/src/chat_channel/command_dispatcher.rs "nexus_chat::shortcut("
revisar src-tauri/src/chat_channel/command_dispatcher.rs "nexus_chat::voice_mode_command("
revisar src-tauri/src/chat_channel/command_dispatcher.rs "nexus_chat::apply_voice_mode("
revisar src-tauri/src/chat_channel/command_dispatcher.rs "nexus_chat::is_ack("
revisar src-tauri/src/commands/mod.rs "nexus_panel"
revisar src-tauri/src/web/handlers/mod.rs "nexus_panel"
revisar src-tauri/src/web/router.rs "handlers::nexus_panel::nexus_panel"
existe src-tauri/src/web/handlers/phantom_route_plan.rs
revisar src-tauri/src/web/handlers/mod.rs "phantom_route_plan"
revisar src-tauri/src/web/router.rs "handlers::phantom_route_plan::phantom_route_plan"
revisar src-tauri/src/lib.rs "commands::nexus_panel::nexus_panel"
revisar src/components/conversations/conversation-detail-panel.tsx "NexusCommandCenter" 2
revisar src/components/chat/message-input.tsx "data-nexus-command-center" 1
revisar src-tauri/src/acp/delegation/listener.rs "arg(\"priority\")" 1
revisar src-tauri/src/chat_channel/session_event_subscriber.rs "nexus_chat::auto_failover(" 1
revisar src-tauri/src/commands/windows.rs "phantom::micro::permitir(" 1
revisar src-tauri/src/chat_channel/backends/telegram.rs "nexus_adjuntos::extraer(" 1
revisar src-tauri/src/acp/connection.rs "phantom::chrome::agregar(" 1
revisar src-tauri/src/automation/engine.rs "phantom::worktrees::path_for(" 2
revisar src-tauri/src/phantom/mod.rs "pub mod worktrees;"
revisar src/components/voice/voice-live-overlay.tsx "useNexusTema()" 1
revisar src/components/phantom/nexus-command-center.tsx "useNexusTema(active)" 1
# Zonas que upstream reescribió en v0.46.0 (barra de estado, lateral, uso, migraciones)
revisar src/components/layout/status-bar.tsx "<PhantomAttentionStatusBar />" 2
revisar src/components/layout/status-bar.tsx "<StatusBarUsage />" 2
revisar src/components/layout/sidebar.tsx "<PhantomActiveSection />" 1
revisar src/components/token-usage/token-usage-page.tsx "ModelScorecardView" 2
revisar src-tauri/src/db/migration/mod.rs "m20261028_000001_phantom_checkpoint::Migration" 1
revisar src-tauri/src/db/migration/mod.rs "m20260924_000001_token_usage_tool_counters::Migration" 1
revisar src/components/chat/message-input.tsx "useVoiceLive(" 1
revisar src/components/chat/message-input.tsx "PromptHistorySearch" 2
revisar src/components/chat/message-input.tsx "usePhantomMemoryComposer(" 1
# Estilo NEXUS (rojo #ff2020 sobre negro) como preset propio y por defecto
revisar src/lib/theme-presets.ts '"nexus",' 1
revisar src/lib/theme-presets.ts 'DEFAULT_THEME_COLOR: ThemeColor = "nexus"' 1
revisar src/app/globals.css '[data-theme="nexus"].dark {' 2
revisar src/components/phantom/attention-status-bar.tsx "useMigrarEstiloNexus()" 1
revisar src/components/voice/voice-galaxy-orb.tsx "readBackgroundColor()" 2
revisar src-tauri/tauri.conf.json '"productName": "Phantom"'

if grep -qE '^(<<<<<<<|>>>>>>>)' -r "$raiz/src" "$raiz/src-tauri/src" "$raiz/src-tauri/tauri.conf.json" 2>/dev/null; then
  echo "FALTA  quedan marcas de conflicto sin resolver"; falta=1
fi
[ "$falta" -eq 0 ] && echo "OK: todos los enganches de NEXUS/Phantom siguen en su sitio"
exit "$falta"
