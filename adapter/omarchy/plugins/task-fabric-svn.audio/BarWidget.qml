import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

BarWidget {
  id: root
  moduleName: "task-fabric-svn.audio"
  property bool popupOpen: false
  property string mode: "full"
  property bool speech: true
  property string statusText: "ready"
  property int masterVolume: 100
  property int speechVolume: 100
  property bool boostApproved: false
  property bool dnd: false
  property bool piperAvailable: false
  property bool kokoroAvailable: false
  property bool llamaAvailable: false
  property bool templatesAvailable: false
  property string speechProfile: "kokoro"
  property int maxSpeechPerMinute: 3
  property int maxSpeechPerTenMinutes: 8
  property bool mediaDucking: true
  property int mediaDuckPercent: 35
  property string humor: "dry"
  property var commandQueue: []

  function run(command) { commandQueue = commandQueue.concat([command]); pumpCommands() }
  function pumpCommands() {
    if (controlProcess.running || commandQueue.length === 0) return
    controlProcess.command = commandQueue[0]
    controlProcess.running = true
  }
  function setMode(value) { mode = value; run(["task-fabric-svn-work", "audio", "mode", value]) }
  function toggleTts() { speech = !speech; run(["task-fabric-svn-work", "audio", "tts", speech ? "on" : "off"]) }
  function preview(value) {
    var request = String(value || "")
    if (request.indexOf("speech ") === 0) run(["task-fabric-svn-work", "audio", "preview", "speech", request.slice(7)])
    else run(["task-fabric-svn-work", "audio", "preview", request])
  }
  function volume(kind, delta) { run(["task-fabric-svn-work", "audio", "volume", kind, "--delta", String(delta)]) }
  function toggleBoost() { run(["task-fabric-svn-work", "audio", "boost", boostApproved ? "revoke" : "approve"]) }
  function toggleDucking() { mediaDucking = !mediaDucking; run(["task-fabric-svn-work", "audio", "ducking", mediaDucking ? "on" : "off"]) }
  function setSpeechProfile(value) { speechProfile = value; run(["task-fabric-svn-work", "audio", "speech", value]) }
  function setHumor(value) { humor = value; run(["task-fabric-svn-work", "audio", "humor", value]) }
  function profileIndex() { return speechProfile === "kokoro" ? 1 : (speechProfile === "piper" ? 2 : (speechProfile === "templates" ? 3 : (speechProfile === "espeak" ? 4 : 0))) }
  function refreshStatus() { if (!statusProcess.running) statusProcess.running = true }
  function parseStatus(raw) {
    try {
      var value = JSON.parse(String(raw || "{}"))
      root.mode = String(value.mode || "full")
      root.speech = value.speech !== false
      root.masterVolume = Number(value.master_volume || 0)
      root.speechVolume = Number(value.speech_volume || 0)
      root.boostApproved = value.boost_approved === true
      root.dnd = value.dnd === true
      root.piperAvailable = value.piper === true && value.voice_configured === true
      root.kokoroAvailable = value.kokoro === true && value.kokoro_model_configured === true
      root.llamaAvailable = value.llama === true && value.model_configured === true
      root.templatesAvailable = value.templates_available === true
      root.speechProfile = String(value.speech_profile || "kokoro")
      root.maxSpeechPerMinute = Number(value.max_speech_per_minute || 3)
      root.maxSpeechPerTenMinutes = Number(value.max_speech_per_10_minutes || 8)
      root.mediaDucking = value.media_ducking !== false
      root.mediaDuckPercent = Number(value.media_duck_percent || 35)
      root.humor = String(value.humor || "dry")
      root.statusText = root.dnd ? "DND" : (root.speechProfile === "auto" ? ((root.templatesAvailable || root.kokoroAvailable || root.piperAvailable) ? "adaptive ready" : "adaptive fallback") : (root.speechProfile === "kokoro" && root.kokoroAvailable ? "Kokoro ready" : (root.speechProfile === "piper" && root.piperAvailable ? "Piper ready" : (root.speechProfile === "templates" && root.templatesAvailable ? "templates ready" : "eSpeak fallback"))))
    } catch (error) { root.statusText = "status unavailable" }
  }
  function modeLabel() { return mode === "sounds-only" ? "sounds" : mode }
  function close() { popupOpen = false }

  visible: true
  implicitWidth: label.implicitWidth + Style.space(16)
  implicitHeight: barSize

  Process {
    id: controlProcess
    command: []
    running: false
    onExited: function(exitCode) {
      if (exitCode !== 0) root.statusText = "unavailable"
      if (root.commandQueue.length > 0) root.commandQueue = root.commandQueue.slice(1)
      root.refreshStatus()
      root.pumpCommands()
    }
  }

  Timer { interval: 5000; repeat: true; running: root.popupOpen; onTriggered: root.refreshStatus() }

  Process {
    id: statusProcess
    command: ["task-fabric-svn-work", "audio", "status", "--json"]
    running: false
    stdout: StdioCollector {
      waitForEnd: true
      onStreamFinished: root.parseStatus(text)
    }
    onExited: function(exitCode) { if (exitCode !== 0) root.statusText = "status unavailable" }
  }

  Row {
    anchors.centerIn: parent
    spacing: Style.space(5)
    Text { text: "◌"; color: root.mode === "muted" ? "#a55555" : "#75f0b0"; font.pixelSize: Style.font.body }
    Text { id: label; text: "AUDIO · " + root.modeLabel(); color: root.bar ? root.bar.barForeground : root.foreground; font.family: root.bar ? root.bar.fontFamily : Style.font.family; font.pixelSize: Style.font.body }
  }

  MouseArea {
    anchors.fill: parent
    cursorShape: Qt.PointingHandCursor
    onClicked: root.popupOpen = !root.popupOpen
    onEntered: if (root.bar) root.bar.showTooltip(root, "Notification audio · " + root.modeLabel())
    onExited: if (root.bar) root.bar.hideTooltip(root)
  }

  PopupCard {
    id: popup
    anchorItem: root
    bar: root.bar
    owner: root
    open: root.popupOpen
    contentWidth: popup.fittedContentWidth(Style.space(350))
    contentHeight: popup.fittedContentHeight(column.implicitHeight)

    Column {
      id: column
      width: parent.width
      spacing: Style.space(8)
      Text { text: "NOTIFICATION AUDIO"; color: Color.notifications.countdown; font.family: Style.font.family; font.pixelSize: Style.font.title }
      Text { text: "Local earcons · " + root.statusText + (root.llamaAvailable ? " · local model ready" : " · model fallback"); color: Color.muted; wrapMode: Text.WordWrap; width: parent.width }
      Text { text: "Speech voice"; color: Color.muted; font.pixelSize: Style.font.bodySmall }
      ComboBox {
        width: parent.width
        currentIndex: root.profileIndex()
        model: ["Adaptive (templates → Kokoro → Piper)", "Kokoro natural", "Piper neural", "Pre-recorded templates", "eSpeak fallback"]
        onActivated: function(index) { root.setSpeechProfile(index === 1 ? "kokoro" : (index === 2 ? "piper" : (index === 3 ? "templates" : (index === 4 ? "espeak" : "auto")))) }
      }
      Text { text: "Humor"; color: Color.muted; font.pixelSize: Style.font.bodySmall }
      Text { text: "Current mode · " + (root.humor === "off" ? "off" : (root.humor === "playful" ? "playful completion phrases" : "dry completion phrases")); color: Color.muted; font.pixelSize: Style.font.caption }
      ComboBox {
        width: parent.width
        currentIndex: root.humor === "off" ? 0 : (root.humor === "playful" ? 2 : 1)
        model: ["Off", "Dry (occasional)", "Playful (occasional)"]
        onActivated: function(index) { root.setHumor(index === 0 ? "off" : (index === 2 ? "playful" : "dry")) }
      }
      Text { text: root.speechProfile === "piper" && !root.piperAvailable ? "Piper voice is not installed; eSpeak will be used until a voice model is added." : (root.speechProfile === "templates" && !root.templatesAvailable ? "Add approval.oga, blocked.oga, and critical.oga under ~/.local/share/task-fabric-svn/templates for exact recordings." : ""); color: Color.muted; wrapMode: Text.WordWrap; width: parent.width; visible: text.length > 0; font.pixelSize: Style.font.caption }
      Text { text: "Speech budget · " + root.maxSpeechPerMinute + "/min · " + root.maxSpeechPerTenMinutes + "/10 min"; color: Color.muted; font.pixelSize: Style.font.caption }
      Row {
        spacing: Style.space(6)
        Text { text: "Media ducking · " + (root.mediaDucking ? root.mediaDuckPercent + "%" : "off"); color: root.mediaDucking ? Color.notifications.text : Color.muted; font.pixelSize: Style.font.bodySmall }
        Rectangle {
          width: Style.space(24); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
          Text { anchors.centerIn: parent; text: "−"; color: Color.notifications.countdown; font.pixelSize: Style.font.body }
          MouseArea { anchors.fill: parent; onClicked: root.run(["task-fabric-svn-work", "audio", "ducking", String(Math.max(0, root.mediaDuckPercent - 10))]) }
        }
        Rectangle {
          width: Style.space(24); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
          Text { anchors.centerIn: parent; text: "+"; color: Color.notifications.countdown; font.pixelSize: Style.font.body }
          MouseArea { anchors.fill: parent; onClicked: root.run(["task-fabric-svn-work", "audio", "ducking", String(Math.min(80, root.mediaDuckPercent + 10))]) }
        }
        Rectangle {
          width: duckAction.implicitWidth + Style.space(12); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
          Text { id: duckAction; anchors.centerIn: parent; text: root.mediaDucking ? "Disable" : "Enable"; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
          MouseArea { anchors.fill: parent; onClicked: root.toggleDucking() }
        }
      }
      Row {
        spacing: Style.space(6)
        Repeater {
          model: ["full", "sounds-only", "muted"]
          delegate: Rectangle {
            required property string modelData
            width: modeText.implicitWidth + Style.space(12); height: Style.space(26); radius: Style.cornerRadius
            color: root.mode === modelData ? Color.notifications.countdown : Color.notifications.background
            Text { id: modeText; anchors.centerIn: parent; text: modelData; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
            MouseArea { anchors.fill: parent; onClicked: root.setMode(modelData) }
          }
        }
      }
      Row {
        spacing: Style.space(6)
        Text { text: "Read-aloud · " + (root.speech ? "on" : "muted"); color: root.speech ? Color.notifications.text : Color.muted; font.pixelSize: Style.font.bodySmall }
        Rectangle {
          width: ttsAction.implicitWidth + Style.space(12); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
          Text { id: ttsAction; anchors.centerIn: parent; text: root.speech ? "Mute" : "Unmute"; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
          MouseArea { anchors.fill: parent; onClicked: root.toggleTts() }
        }
      }
      Text { text: "Volume"; color: Color.muted; font.pixelSize: Style.font.bodySmall }
      Row {
        spacing: Style.space(6)
        Repeater {
          model: ["master", "speech"]
          delegate: Row {
            required property string modelData
            spacing: Style.space(3)
            Text { text: modelData + " " + (modelData === "master" ? root.masterVolume : root.speechVolume) + "%"; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
            Rectangle {
              width: Style.space(24); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
              Text { anchors.centerIn: parent; text: "−"; color: Color.notifications.countdown; font.pixelSize: Style.font.body }
              MouseArea { anchors.fill: parent; onClicked: root.volume(modelData, -10) }
            }
            Rectangle {
              width: Style.space(24); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
              Text { anchors.centerIn: parent; text: "+"; color: Color.notifications.countdown; font.pixelSize: Style.font.body }
              MouseArea { anchors.fill: parent; onClicked: root.volume(modelData, 10) }
            }
          }
        }
      }
      Row {
        spacing: Style.space(6)
        Text { text: root.boostApproved ? "Boost approved · up to 150%" : "Boost locked · max 100%"; color: root.boostApproved ? "#75f0b0" : Color.muted; font.pixelSize: Style.font.bodySmall }
        Rectangle {
          width: boostAction.implicitWidth + Style.space(12); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
          Text { id: boostAction; anchors.centerIn: parent; text: root.boostApproved ? "Revoke" : "Approve boost"; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
          MouseArea { anchors.fill: parent; onClicked: root.toggleBoost() }
        }
      }
      Text { text: "Preview"; color: Color.muted; font.pixelSize: Style.font.bodySmall }
      Row {
        spacing: Style.space(4)
        Repeater {
          model: ["arrival", "message", "complete", "approval", "blocked", "critical"]
          delegate: Rectangle {
            required property string modelData
            width: previewText.implicitWidth + Style.space(10); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
            Text { id: previewText; anchors.centerIn: parent; text: modelData; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
            MouseArea { anchors.fill: parent; onClicked: root.preview(modelData) }
          }
        }
      }
      Text { text: "Speech previews"; color: Color.muted; font.pixelSize: Style.font.bodySmall }
      Row {
        spacing: Style.space(4)
        Repeater {
          model: ["approval", "authentication", "blocked", "critical", "humor"]
          delegate: Rectangle {
            required property string modelData
            width: speechText.implicitWidth + Style.space(10); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
            Text { id: speechText; anchors.centerIn: parent; text: modelData; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
            MouseArea { anchors.fill: parent; onClicked: root.preview("speech " + modelData) }
          }
        }
      }
      Text { text: "Agent Voice Personas (Kokoro)"; color: Color.muted; font.pixelSize: Style.font.bodySmall }
      Row {
        spacing: Style.space(4)
        Repeater {
          model: [
            { name: "Claude", voice: "af_heart" },
            { name: "Codex", voice: "am_adam" },
            { name: "Gemini", voice: "bf_emma" },
            { name: "OpenCode", voice: "am_echo" }
          ]
          delegate: Rectangle {
            required property var modelData
            width: personaText.implicitWidth + Style.space(10); height: Style.space(24); radius: Style.cornerRadius; color: Color.notifications.background
            Text { id: personaText; anchors.centerIn: parent; text: modelData.name + " · " + modelData.voice; color: Color.notifications.text; font.pixelSize: Style.font.bodySmall }
            MouseArea { anchors.fill: parent; onClicked: root.preview("speech approval") }
          }
        }
      }
    }
  }

  Component.onCompleted: root.refreshStatus()
}
