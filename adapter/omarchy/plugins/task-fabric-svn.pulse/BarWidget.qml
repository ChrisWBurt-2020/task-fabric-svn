import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

// Presentation-only adapter. Project discovery, state, and all mutations
// belong to task-fabric-svn-work; this widget only consumes status JSON and invokes
// fixed, identifier-based CLI actions.
BarWidget {
  id: root

  moduleName: "task-fabric-svn.pulse"

  property var snapshot: null
  property bool popupOpen: false
  property string errorText: ""
  property string actionStatus: ""
  property string signalNote: ""
  property var resumePlan: null

  readonly property color foreground: bar ? bar.foreground : Color.foreground
  readonly property color mutedForeground: Qt.darker(foreground, 1.25)

  function objectValue(value) {
    return value && typeof value === "object" && !Array.isArray(value) ? value : ({})
  }

  function listValue(value) {
    if (Array.isArray(value)) return value
    if (!value || typeof value !== "object") return []

    var result = []
    for (var key in value) {
      var item = value[key]
      if (item && typeof item === "object") {
        var copy = {}
        for (var field in item) copy[field] = item[field]
        if (!copy.id) copy.id = key
        result.push(copy)
      } else {
        result.push({ id: key, status: String(item) })
      }
    }
    return result
  }

  function firstString(values, fallback) {
    for (var i = 0; i < values.length; i++) {
      var value = values[i]
      if (value !== undefined && value !== null && String(value) !== "") return String(value)
    }
    return fallback || ""
  }

  function projectObject() {
    var value = snapshot ? snapshot.project : null
    if (typeof value === "string") return { id: value, label: value }
    if (value && typeof value === "object") return value

    value = snapshot ? (snapshot.activeProject || snapshot.active_project) : null
    if (typeof value === "string") return { id: value, label: value }
    return objectValue(value)
  }

  function gitObject() {
    var project = projectObject()
    return objectValue((snapshot && snapshot.git) || project.git)
  }

  function herdrObject() {
    var project = projectObject()
    return objectValue((snapshot && (snapshot.workspace || snapshot.herdr)) || project.workspace || project.herdr)
  }

  function statusArray(key) {
    var value = snapshot ? snapshot[key] : null
    if (value && !Array.isArray(value) && typeof value === "object") {
      if (Array.isArray(value.items)) value = value.items
      else if (Array.isArray(value.list)) value = value.list
    }
    return listValue(value)
  }

  readonly property var project: root.projectObject()
  readonly property var projectList: root.statusArray("projects")
  readonly property var git: root.gitObject()
  readonly property var workspace: root.herdrObject()
  readonly property var agentList: root.statusArray("agents")
  readonly property var serviceList: root.statusArray("services")
  readonly property var attentionList: root.statusArray("attention")
  readonly property var actionList: root.statusArray("actions")
  readonly property var durable: objectValue(snapshot ? snapshot.durable_value : null)
  readonly property string durableLevel: root.firstString([durable.level], "quiet")
  readonly property string durableSummary: root.firstString([durable.summary], "No significant durable-value drift detected.")
  readonly property var durableReasons: Array.isArray(durable.reasons) ? durable.reasons : []
  readonly property var positiveSignals: Array.isArray(durable.recent_positive_signals) ? durable.recent_positive_signals : []

  readonly property string projectId: root.firstString([
    project.id, project.projectId, project.project_id,
    snapshot ? snapshot.project_id : "", snapshot ? snapshot.projectId : ""
  ], "")
  readonly property string projectLabel: root.firstString([
    project.label, project.name, project.id,
    snapshot ? snapshot.project_label : ""
  ], "No project")
  readonly property string branch: root.firstString([
    git.branch, git.name, project.branch,
    snapshot ? snapshot.branch : ""
  ], "no branch")
  readonly property string phase: root.firstString([
    snapshot ? snapshot.phase : "", project.phase
  ], "")
  readonly property string objective: root.firstString([
    snapshot ? snapshot.objective : "", project.objective
  ], "No objective recorded")
  readonly property string nextAction: root.firstString([
    snapshot ? snapshot.next_action : "",
    snapshot ? snapshot.nextAction : "",
    project.next_action, project.nextAction
  ], "Choose the next meaningful action")
  readonly property string workspaceLabel: root.firstString([
    workspace.session, workspace.session_name, workspace.herdr_session,
    project.herdr_session, project.herdrSession,
    snapshot ? snapshot.herdr_session : ""
  ], "No persistent session")
  readonly property bool dirty: git.dirty === true
    || git.clean === false
    || (snapshot && snapshot.dirty === true)
  readonly property int attentionCount: root.attentionList.filter(function(item) {
    return item && ["warning", "error", "critical"].indexOf(String(item.severity || "")) >= 0
  }).length
    || Number(snapshot && (snapshot.attention_count || snapshot.attentionCount) || 0)
  readonly property bool hasAttention: attentionCount > 0
  readonly property int agentCount: root.agentList.length
  readonly property string phaseOrBranch: phase !== "" ? phase : branch
  readonly property string cleanlinessLabel: dirty ? "dirty" : (hasAttention ? "attention" : "clean")
  readonly property string attentionReason: root.dirty
    ? "working tree has changes"
    : (root.attentionList.length > 0 ? root.attentionLabel(root.attentionList[0]) : "none")
  readonly property string barTooltip: root.errorText !== ""
    ? "Task Fabric SVN workstation unavailable"
    : (root.hasAttention
      ? "Attention: " + root.attentionReason + " · workstation connected"
      : "Task Fabric SVN workstation connected · no attention")

  readonly property int refreshIntervalSec: Math.max(2,
    Number(setting("refreshIntervalSec", 10)) || 10)

  function refresh() {
    if (!statusProcess.running) statusProcess.running = true
  }

  function requestResumePlan() {
    if (!safeIdentifier(projectId) || planProcess.running) return
    root.actionStatus = "Preparing resume…"
    planProcess.command = ["task-fabric-svn-work", "enter", projectId, "--plan", "--json"]
    planProcess.running = true
  }

  function confirmResume() {
    if (!root.resumePlan || !safeIdentifier(projectId)) return
    root.resumePlan = null
    runCommand(["task-fabric-svn-work", "enter", projectId, "--json"])
  }

  function parseResumePlan(raw) {
    try {
      var value = JSON.parse(String(raw || ""))
      if (!value || !Array.isArray(value.effects)) throw new Error("invalid resume plan")
      root.resumePlan = value
      root.actionStatus = "Review the resume effects, then confirm."
    } catch (error) {
      root.resumePlan = null
      root.actionStatus = "Resume plan unavailable"
    }
  }

  function parseStatus(raw) {
    try {
      var response = JSON.parse(String(raw || ""))
      var payload = response && response.result !== undefined ? response.result
        : (response && response.data !== undefined ? response.data : response)
      if (!payload || typeof payload !== "object") throw new Error("invalid status response")
      root.snapshot = payload
      root.errorText = ""
    } catch (error) {
      root.snapshot = null
      root.errorText = "task-fabric-svn-work status unavailable"
    }
  }

  function safeIdentifier(value) {
    var text = String(value || "")
    return text.length > 0 && text.length <= 128
      && /^[A-Za-z0-9._:@+-]+$/.test(text)
  }

  function runCommand(command) {
    if (!Array.isArray(command) || command.length === 0 || controlProcess.running) return
    controlProcess.command = command
    root.actionStatus = "Running…"
    controlProcess.running = true
  }

  function runProjectAction(action, extraArgs) {
    if (!safeIdentifier(projectId) || !safeIdentifier(action)) {
      root.actionStatus = "No active project"
      return
    }

    var command = ["task-fabric-svn-work", "action", "run", projectId, action]
    var extras = Array.isArray(extraArgs) ? extraArgs : []
    for (var i = 0; i < extras.length; i++) {
      if (!safeIdentifier(extras[i])) {
        root.actionStatus = "Invalid action identifier"
        return
      }
      command.push(String(extras[i]))
    }
    runCommand(command)
  }

  function recordSignal(kind) {
    var allowed = ["shipped", "deployed", "external-user", "returning-user", "customer-request", "payment", "feature-deleted", "platform-substitution", "usage-observed"]
    if (allowed.indexOf(kind) < 0 || !safeIdentifier(projectId)) {
      root.actionStatus = "No active project"
      return
    }
    var command = ["task-fabric-svn-work", "signal", "record", projectId, kind]
    if (root.signalNote.length > 0) command.push("--note", root.signalNote.slice(0, 240))
    runCommand(command)
    root.signalNote = ""
  }

  function durableColor() {
    if (root.durableLevel === "elevated") return "#f28f8f"
    if (root.durableLevel === "notice") return "#f2c14e"
    return "#75f0b0"
  }

  function workOnProject() {
    if (root.resumePlan) confirmResume()
    else requestResumePlan()
  }

  function selectProject(id) {
    if (safeIdentifier(id)) {
      root.resumePlan = null
      runCommand(["task-fabric-svn-work", "select", String(id)])
    }
    else root.actionStatus = "Invalid project identifier"
  }

  function focusAgent(agent) {
    var id = agent && (agent.id || agent.agentId || agent.agent_id || agent.name)
    if (safeIdentifier(id)) runProjectAction("focus-agent", [id])
    else root.actionStatus = "Agent has no safe identifier"
  }

  function agentStateColor(state) {
    var s = String(state || "").toLowerCase()
    if (s.indexOf("approval") >= 0 || s.indexOf("input") >= 0) return "#f59e0b"
    if (s.indexOf("running") >= 0 || s.indexOf("active") >= 0 || s.indexOf("working") >= 0) return "#10b981"
    if (s.indexOf("blocked") >= 0 || s.indexOf("error") >= 0 || s.indexOf("failed") >= 0) return "#ef4444"
    return "#60a5fa"
  }

  function agentLabel(agent) {
    var dur = agent && agent.duration ? " [" + agent.duration + "]" : ""
    return root.firstString([agent.label, agent.name, agent.provider, agent.id], "Agent")
      + " · " + root.firstString([agent.state, agent.status], "unknown") + dur
  }

  function serviceLabel(service) {
    return root.firstString([service.label, service.name, service.id], "Service")
      + " · " + root.firstString([service.health, service.status, service.state], "unknown")
  }

  function attentionLabel(item) {
    if (typeof item === "string") return item
    return root.firstString([item.title, item.summary, item.message, item.reason, item.id], "Attention required")
  }

  function close() { popupOpen = false }

  visible: true
  // Keep the bar compact; the full project/agent state remains in the popup.
  implicitWidth: Style.space(34)
  implicitHeight: barSize

  Process {
    id: statusProcess
    command: ["task-fabric-svn-work", "status", "--json"]

    stdout: StdioCollector {
      id: statusStdout
      waitForEnd: true
      onStreamFinished: root.parseStatus(text)
    }

    stderr: StdioCollector {
      id: statusStderr
      waitForEnd: true
    }

    onExited: function(exitCode) {
      if (exitCode !== 0) {
        root.errorText = String(statusStderr.text || "task-fabric-svn-work status unavailable").trim()
      }
    }
  }

  Process {
    id: controlProcess
    command: []

    stdout: StdioCollector {
      id: controlStdout
      waitForEnd: true
    }

    stderr: StdioCollector {
      id: controlStderr
      waitForEnd: true
    }

    onExited: function(exitCode) {
      var error = String(controlStderr.text || "").trim()
      root.actionStatus = exitCode === 0 ? "Done" : (error || "Action failed")
      root.refresh()
    }
  }

  Process {
    id: planProcess
    command: []

    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.parseResumePlan(text)
    }

    stderr: StdioCollector { waitForEnd: true }
  }

  Timer {
    interval: root.refreshIntervalSec * 1000
    running: true
    repeat: true
    onTriggered: root.refresh()
  }

  Component.onCompleted: root.refresh()

  Row {
    anchors.fill: parent
    anchors.leftMargin: Style.space(8)
    anchors.rightMargin: Style.space(8)
    spacing: Style.space(5)

    Text {
      id: label
      text: "◆"
      anchors.centerIn: parent
      color: root.errorText !== "" ? "#a55555"
        : root.hasAttention ? "#f2c14e" : "#75f0b0"
      font.family: root.bar ? root.bar.fontFamily : Style.font.family
      font.pixelSize: Style.font.body
      opacity: root.errorText !== "" ? 0.65 : 1.0
    }
  }

  MouseArea {
    anchors.fill: parent
    hoverEnabled: true
    cursorShape: Qt.PointingHandCursor
    onClicked: root.popupOpen = !root.popupOpen
    onEntered: if (root.bar) root.bar.showTooltip(root, root.barTooltip + " · click for details")
    onExited: if (root.bar) root.bar.hideTooltip(root)
  }

  PopupCard {
    id: popup
    anchorItem: root
    bar: root.bar
    owner: root
    open: root.popupOpen
    contentWidth: popup.fittedContentWidth(Style.space(420))
    contentHeight: popup.fittedContentHeight(column.implicitHeight)

    Flickable {
      id: scroller
      anchors.fill: parent
      contentWidth: width
      contentHeight: column.implicitHeight
      clip: true
      interactive: contentHeight > height
      boundsBehavior: Flickable.StopAtBounds

      Column {
        id: column
        width: scroller.width
        spacing: Style.space(8)

        Text {
          text: "TASK FABRIC SVN PULSE"
          color: root.foreground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.subtitle
          font.bold: true
          width: parent.width
        }

        Text {
          text: root.errorText !== "" ? root.errorText : "Live workstation context"
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.Wrap
          width: parent.width
        }

        Text {
          text: "NOW"
          color: root.foreground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          font.bold: true
          width: parent.width
        }

        Text {
          text: root.projectLabel + " · " + root.branch
            + (root.phase !== "" ? " · phase " + root.phase : "")
            + "\nObjective: " + root.objective
            + "\nNext: " + root.nextAction
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.WrapAnywhere
          width: parent.width
        }

        Text {
          visible: root.snapshot && root.snapshot.suggested_project
          text: root.snapshot && root.snapshot.suggested_project
            ? "Suggested project: " + root.snapshot.suggested_project.label
              + " · " + root.snapshot.suggested_project.reason : ""
          color: "#f2c14e"
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.WrapAnywhere
          width: parent.width
        }

        Flow {
          width: parent.width
          spacing: Style.space(6)
          height: childrenRect.height

          Repeater {
            model: root.projectList
            delegate: Button {
              required property var modelData
              text: root.firstString([modelData.label, modelData.id], "Project")
              foreground: root.foreground
              enabled: root.safeIdentifier(modelData.id) && modelData.id !== root.projectId
              onClicked: root.selectProject(modelData.id)
            }
          }
        }

        Text { text: "TASK FABRIC SVN TEST · " + root.durableLevel.toUpperCase(); color: root.durableColor(); font.family: root.bar ? root.bar.fontFamily : Style.font.family; font.pixelSize: Style.font.bodySmall; font.bold: true; width: parent.width }
        Text { text: root.durableSummary; color: root.mutedForeground; font.family: root.bar ? root.bar.fontFamily : Style.font.family; font.pixelSize: Style.font.bodySmall; wrapMode: Text.Wrap; width: parent.width }

        Text {
          text: "AGENTS"
          color: root.foreground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          font.bold: true
          width: parent.width
        }

        Text {
          visible: root.agentList.length === 0
          text: "No agent sessions reported"
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          width: parent.width
        }

        Repeater {
          model: root.agentList
          delegate: Row {
            required property var modelData
            width: parent ? parent.width : 0
            spacing: Style.space(6)

            Rectangle {
              width: 7
              height: 7
              radius: 3.5
              color: root.agentStateColor(modelData.state)
              anchors.verticalCenter: parent.verticalCenter
            }

            Text {
              text: root.agentLabel(modelData)
              color: root.mutedForeground
              font.family: root.bar ? root.bar.fontFamily : Style.font.family
              font.pixelSize: Style.font.bodySmall
              elide: Text.ElideRight
              width: Math.max(0, parent.width - resumeButton.implicitWidth - parent.spacing * 2 - 7)
              anchors.verticalCenter: parent.verticalCenter
            }

            Button {
              id: resumeButton
              text: "Open"
              foreground: root.foreground
              onClicked: root.focusAgent(modelData)
            }
          }
        }

        Text {
          text: "SERVICES"
          color: root.foreground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          font.bold: true
          width: parent.width
        }

        Text {
          visible: root.serviceList.length === 0
          text: "No declared services"
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          width: parent.width
        }

        Repeater {
          model: root.serviceList
          delegate: Text {
            required property var modelData
            text: root.serviceLabel(modelData)
            color: root.mutedForeground
            font.family: root.bar ? root.bar.fontFamily : Style.font.family
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WrapAnywhere
            width: parent ? parent.width : 0
          }
        }

        Text {
          text: "WORKSPACE"
          color: root.foreground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          font.bold: true
          width: parent.width
        }

        Text {
          text: root.workspaceLabel
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          width: parent.width
          elide: Text.ElideRight
        }

        Flow {
          width: parent.width
          spacing: Style.space(6)
          height: childrenRect.height

          Button {
            text: root.resumePlan ? "Confirm resume" : "Resume project"
            foreground: root.foreground
            enabled: root.safeIdentifier(root.projectId)
            onClicked: root.workOnProject()
          }

          Button {
            text: "Refresh"
            foreground: root.foreground
            onClicked: root.refresh()
          }
        }

        Text {
          text: "ATTENTION"
          color: root.foreground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          font.bold: true
          width: parent.width
        }

        Text {
          visible: root.attentionCount === 0
          text: "Nothing requires attention"
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          width: parent.width
        }

        Text {
          visible: root.dirty
          text: "Git worktree is dirty (context only)"
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          width: parent.width
        }

        Repeater {
          model: root.attentionList
          delegate: Text {
            required property var modelData
            text: "• " + root.attentionLabel(modelData)
            color: "#f2c14e"
            font.family: root.bar ? root.bar.fontFamily : Style.font.family
            font.pixelSize: Style.font.bodySmall
            wrapMode: Text.WrapAnywhere
            width: parent ? parent.width : 0
          }
        }

        Text {
          text: "ACTIONS"
          color: root.foreground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          font.bold: true
          width: parent.width
        }

        Flow {
          width: parent.width
          spacing: Style.space(6)
          height: childrenRect.height

          Repeater {
            model: root.actionList
            delegate: Button {
              required property var modelData
              text: root.firstString([modelData.label, modelData.id], "Action")
              foreground: root.foreground
              enabled: root.safeIdentifier(modelData.id)
              onClicked: root.runProjectAction(modelData.id)
            }
          }
        }

        Text {
          visible: root.resumePlan && Array.isArray(root.resumePlan.effects)
          text: root.resumePlan && Array.isArray(root.resumePlan.effects)
            ? "Resume effects:\n" + root.resumePlan.effects.map(function(effect) {
                return "• " + root.firstString([effect.kind, effect.id], "effect")
                  + " → " + root.firstString([effect.target_ref, effect.target], "target")
              }).join("\n") : ""
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.WrapAnywhere
          width: parent.width
        }

        Text {
          visible: root.actionStatus !== ""
          text: root.actionStatus
          color: root.mutedForeground
          font.family: root.bar ? root.bar.fontFamily : Style.font.family
          font.pixelSize: Style.font.bodySmall
          wrapMode: Text.WrapAnywhere
          width: parent.width
        }
      }
    }
  }
}
