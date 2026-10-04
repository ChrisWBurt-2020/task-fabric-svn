#!/usr/bin/env bash
set -euo pipefail

repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
plugin_dir="$repo_root/adapter/omarchy/plugins/task-fabric-svn.pulse"
audio_plugin_dir="$repo_root/adapter/omarchy/plugins/task-fabric-svn.audio"

jq -e '
  .schemaVersion == 1
  and .id == "task-fabric-svn.pulse"
  and (.kinds | index("bar-widget"))
  and .entryPoints.barWidget == "BarWidget.qml"
  and (.barWidget.defaultSection | IN("left", "center", "right"))
' "$plugin_dir/manifest.json" >/dev/null

for file in "$plugin_dir/BarWidget.qml"; do
  test -s "$file"
  rg -q '^import qs\.Ui$' "$file"
  rg -q 'command: \["task-fabric-svn-work", "status", "--json"\]' "$file"
  rg -q 'function runProjectAction' "$file"
  rg -q 'readonly property var actionList: root.statusArray\("actions"\)' "$file"
  rg -q 'model: root.actionList' "$file"
  rg -q 'function requestResumePlan' "$file"
  ! rg -q 'text: "Open Task Fabric SVN"|text: "Run Task Fabric SVN Test"' "$file"
  rg -q 'PopupCard \{' "$file"
done

jq -e '.schemaVersion == 1 and .id == "task-fabric-svn.audio" and (.kinds | index("bar-widget")) and .entryPoints.barWidget == "BarWidget.qml"' "$audio_plugin_dir/manifest.json" >/dev/null
test -s "$audio_plugin_dir/BarWidget.qml"

if command -v qmllint >/dev/null 2>&1; then
  # The Omarchy shell supplies qs.Commons and qs.Ui at runtime. Bare linting
  # still catches malformed QML syntax without requiring a running shell.
  qmllint --bare "$plugin_dir/BarWidget.qml" || {
    echo "qmllint reported issues; imports may require the running Omarchy shell" >&2
    exit 1
  }
  qmllint --bare "$audio_plugin_dir/BarWidget.qml" || {
    echo "qmllint reported issues in task-fabric-svn.audio; imports may require the running Omarchy shell" >&2
    exit 1
  }
fi

echo "Omarchy Task Fabric SVN Pulse adapter validation passed"

command -v notify-send >/dev/null || {
  echo "notify-send is required for Task Fabric SVN Pulse desktop alerts" >&2
  exit 1
}
command -v pw-play >/dev/null || {
  echo "pw-play is required for Task Fabric SVN adaptive audible alerts" >&2
  exit 1
}

echo "Omarchy user-session notification and sound tools are available"
