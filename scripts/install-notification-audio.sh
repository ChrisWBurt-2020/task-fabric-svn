#!/usr/bin/env bash
set -euo pipefail

target=${1:-${XDG_CONFIG_HOME:-$HOME/.config}/omarchy/plugins/user.notifications/Service.qml}
[[ -f "$target" ]] || { echo "Notification plugin not found; leaving it untouched: $target"; exit 0; }
python3 - "$target" <<'PY'
from pathlib import Path
import re
import sys

path = Path(sys.argv[1])
text = path.read_text()
if "TASK-FABRIC-SVN-AUDIO-INTEGRATION" in text:
    text = text.replace('  property bool audioBusy: false\n  property string audioPayload: ""', '  property var audioQueue: []\n  property string audioPayload: ""')
    text = text.replace('    if (service.audioBusy) return\n', '')
    text = text.replace('    service.audioPayload = JSON.stringify({', '    var payload = JSON.stringify({')
    text = text.replace('    service.audioBusy = true\n    audioProcess.running = true\n  }', '    service.audioQueue = service.audioQueue.concat([payload])\n    pumpAudioQueue()\n  }\n\n  function pumpAudioQueue() {\n    if (audioProcess.running || service.audioQueue.length === 0) return\n    service.audioPayload = service.audioQueue[0]\n    audioProcess.running = true\n  }', 1)
    text = text.replace('    onRunningChanged: if (!running) service.audioBusy = false', '    onRunningChanged: if (!running && service.audioQueue.length > 0) {\n      service.audioQueue = service.audioQueue.slice(1)\n      service.pumpAudioQueue()\n    }')
    path.write_text(text)
    raise SystemExit(0)
old = re.compile(r'\n  readonly property string attentionSound:.*?\n  function durationFor\(', re.S)
replacement = r'''
  property var audioQueue: []
  property string audioPayload: ""

  function audioUrgency(notification) {
    var value = Number(notification && notification.urgency)
    return value >= NotificationUrgency.Critical ? "critical" : (value <= NotificationUrgency.Low ? "low" : "normal")
  }

  function audioSanitize(value) {
    return String(value || "").replace(/<[^>]*>/g, "").replace(/https?:\/\/\S+/gi, "[link]").replace(/(^|\s)(\/|~\/)[^\s]+/g, " [path] ").slice(0, 1200)
  }

  // TASK-FABRIC-SVN-AUDIO-INTEGRATION
  function enqueueAudio(notification) {
    var hints = notification && notification.hints ? notification.hints : ({})
    var category = String(hints["task-fabric-svn-audio-category"] || "")
    var need = String(hints["task-fabric-svn-audio-need"] || "")
    var context = String(hints["task-fabric-svn-audio-context"] || "")
    var outcome = String(hints["task-fabric-svn-audio-outcome"] || "")
    var task_fabric_svn = (category || need || context || outcome) ? { category: category, need: need, context: context, outcome: outcome } : null
    var payload = JSON.stringify({schema_version: 1, id: String(notification && notification.id || Date.now()), replaces_id: notification && notification.replacesId ? String(notification.replacesId) : null, timestamp_epoch_ms: Date.now(), app: audioSanitize(notification && notification.appName), urgency: audioUrgency(notification), summary: audioSanitize(notification && notification.summary), body: audioSanitize(notification && notification.body), dnd: service.doNotDisturb, task_fabric_svn: task_fabric_svn})
    service.audioQueue = service.audioQueue.concat([payload])
    pumpAudioQueue()
  }

  function pumpAudioQueue() {
    if (audioProcess.running || service.audioQueue.length === 0) return
    service.audioPayload = service.audioQueue[0]
    audioProcess.running = true
  }

  function durationFor('''
if not old.search(text):
    raise SystemExit("notification plugin shape is newer than the known patch; no changes made")
text = old.sub(replacement, text, count=1)
text = text.replace('    if (needsAttentionSound(notification)) playAttentionSound()', '    enqueueAudio(notification)', 1)
needle = '  Process { id: focusAppProc; running: false }'
process = '''  Process { id: focusAppProc; running: false }

  Process {
    id: audioProcess
    command: ["task-fabric-svn-work", "audio", "notify"]
    stdinEnabled: true
    running: false
    onStarted: write(service.audioPayload + "\\n")
    onRunningChanged: if (!running && service.audioQueue.length > 0) {
      service.audioQueue = service.audioQueue.slice(1)
      service.pumpAudioQueue()
    }
  }'''
if needle not in text:
    raise SystemExit("notification plugin process anchor not found; no changes made")
text = text.replace(needle, process, 1)
path.write_text(text)
PY
echo "Installed Task Fabric SVN notification audio integration: $target"
