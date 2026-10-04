#!/usr/bin/env bash
set -euo pipefail

# Patch the user-owned Omarchy polkit plugin so native authentication prompts
# use the same local notification-audio owner as FreeDesktop notifications.
# This deliberately never edits /usr/share/omarchy.
target=${1:-${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/user.polkit/PolkitAgent.qml}
[[ -f "$target" ]] || {
  printf 'Polkit plugin not found; leaving it untouched: %s\n' "$target"
  exit 0
}

python3 - "$target" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
text = path.read_text()
if "TASK-FABRIC-SVN-POLKIT-AUDIO-INTEGRATION" in text or "polkitAudioPayload" in text:
    print(f"Polkit audio integration already present: {path}")
    raise SystemExit(0)

property_anchor = "  property int fieldHeight: Math.max(Style.space(42), Style.spacing.controlHeight)\n"
property_replacement = property_anchor + (
    "  // TASK-FABRIC-SVN-POLKIT-AUDIO-INTEGRATION\n"
    "  property string polkitAudioPayload: \"\"\n"
    "  property string lastAuthenticationAudioKey: \"\"\n"
    "  property bool notificationDnd: false\n"
)
if property_anchor not in text:
    raise SystemExit("polkit plugin shape is newer than the known patch; no changes made")
text = text.replace(property_anchor, property_replacement, 1)

reset_anchor = "  function resetSnapshot() {\n"
audio_functions = (
    "  function loadNotificationDnd(raw) {\n"
    "    try {\n"
    "      var parsed = JSON.parse(String(raw || \"{}\"))\n"
    "      root.notificationDnd = parsed && parsed.dnd === true\n"
    "    } catch (error) {\n"
    "      root.notificationDnd = false\n"
    "    }\n"
    "  }\n\n"
    "  function queueAuthenticationAudio() {\n"
    "    var flow = polkitAgent.flow\n"
    "    var key = String(flow && flow.message || \"Authentication required\") + \"|\" + String(flow && flow.inputPrompt || \"\")\n"
    "    if (key === root.lastAuthenticationAudioKey) return\n"
    "    root.lastAuthenticationAudioKey = key\n"
    "    root.polkitAudioPayload = JSON.stringify({\n"
    "      schema_version: 1,\n"
    "      id: \"polkit-auth-\" + Date.now(),\n"
    "      timestamp_epoch_ms: Date.now(),\n"
    "      app: \"Polkit\",\n"
    "      urgency: \"normal\",\n"
    "      summary: \"Authentication required\",\n"
    "      body: \"\",\n"
    "      dnd: root.notificationDnd\n"
    "    })\n"
    "    if (!polkitAudioProcess.running) polkitAudioProcess.running = true\n"
    "  }\n\n"
)
if reset_anchor not in text:
    raise SystemExit("polkit resetSnapshot anchor not found; no changes made")
text = text.replace(reset_anchor, audio_functions + reset_anchor, 1)

begin_anchor = "    syncFromFlow()\n    Qt.callLater(refocus)"
begin_replacement = (
    "    syncFromFlow()\n"
    "    // Native Polkit prompts do not emit a FreeDesktop notification. Send a\n"
    "    // bounded local event through task-fabric-svn-soundd for the configured audio.\n"
    "    queueAuthenticationAudio()\n"
    "    Qt.callLater(refocus)"
)
if begin_anchor not in text:
    raise SystemExit("polkit beginFlow anchor not found; no changes made")
text = text.replace(begin_anchor, begin_replacement, 1)

process_anchor = "  Process {\n    id: laptopClosedProc\n"
process_replacement = (
    "  FileView {\n"
    "    path: Quickshell.env(\"HOME\") + \"/.local/state/omarchy/notifications.json\"\n"
    "    watchChanges: true\n"
    "    printErrors: false\n"
    "    onLoaded: root.loadNotificationDnd(text())\n"
    "    onLoadFailed: root.notificationDnd = false\n"
    "    onFileChanged: reload()\n"
    "  }\n\n"
    "  Process {\n"
    "    id: polkitAudioProcess\n"
    "    command: [\"task-fabric-svn-work\", \"audio\", \"notify\"]\n"
    "    stdinEnabled: true\n"
    "    running: false\n"
    "    onStarted: write(root.polkitAudioPayload + \"\\n\")\n"
    "    onExited: root.polkitAudioPayload = \"\"\n"
    "  }\n\n"
    "  Process {\n    id: laptopClosedProc\n"
)
if process_anchor not in text:
    raise SystemExit("polkit process anchor not found; no changes made")
text = text.replace(process_anchor, process_replacement, 1)

active_anchor = (
    "    onIsActiveChanged: {\n"
    "      if (isActive) root.syncFromFlow()\n"
    "      else if (!root.closing) root.resetSnapshot()\n"
    "    }\n"
)
active_replacement = (
    "    onIsActiveChanged: {\n"
    "      if (isActive) root.syncFromFlow()\n"
    "      else {\n"
    "        root.lastAuthenticationAudioKey = \"\"\n"
    "        if (!root.closing) root.resetSnapshot()\n"
    "      }\n"
    "    }\n"
)
if active_anchor not in text:
    raise SystemExit("polkit active-state anchor not found; no changes made")
text = text.replace(active_anchor, active_replacement, 1)

path.write_text(text)
print(f"Installed Task Fabric SVN polkit audio integration: {path}")
PY
