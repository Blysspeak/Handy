// Diktor voice mode in the Omarchy bar (Hyprland). Install as
// ~/.config/omarchy/bar/modules/diktor.qml and add { "id": "diktor", "type": "qml" }
// to bar.layout in ~/.config/omarchy/shell.json.
//
// State comes from $XDG_RUNTIME_DIR/diktor/voice-state.json, which Diktor
// replaces atomically (voice_session/bar_state.rs): {state, tooltip, draft}.
// Left click toggles voice mode; right click copies an unsent draft.
import QtQuick
import Quickshell
import Quickshell.Io
import qs.Commons
import qs.Ui

Item {
  id: root

  property var bar
  property string moduleName
  property var settings

  // "off" | "listening" | "speech" | "sending" | "error"; `state` is taken by Item.
  property string voiceState: "off"
  property string tooltip: ""
  property string draft: ""

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function apply(raw) {
    var data = {}
    try { data = JSON.parse(raw) } catch (e) {}
    voiceState = String(data.state || "off")
    tooltip = String(data.tooltip || "")
    draft = String(data.draft || "")
  }

  FileView {
    id: stateFile
    path: Quickshell.env("XDG_RUNTIME_DIR") + "/diktor/voice-state.json"
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: root.apply(text())
    onLoadFailed: {
      root.apply("")
      retry.start()
    }
  }

  // A missing file is never watched, and the bar usually starts before
  // Diktor creates it: look again until it exists, then changes are pushed.
  Timer {
    id: retry
    interval: 1000
    onTriggered: stateFile.reload()
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰍬"
    dimmed: root.voiceState === "off"
    active: root.voiceState === "speech" || root.voiceState === "sending" || root.voiceState === "error"
    activeColor: root.voiceState === "error" ? Color.urgent : Color.accent
    tooltipText: root.tooltip ? "Diktor\n" + root.tooltip : "Diktor"
    onPressed: function(b) {
      if (!root.bar) return
      if (b === Qt.LeftButton) root.bar.run("\"$HOME/.local/bin/handy\" --toggle-voice-mode")
      else if (b === Qt.RightButton && root.draft) root.bar.run("wl-copy -- " + root.bar.shellQuote(root.draft))
    }
  }

  // The only animation: a subtle breath while speech is heard.
  SequentialAnimation on opacity {
    running: root.voiceState === "speech"
    loops: Animation.Infinite
    alwaysRunToEnd: true
    NumberAnimation { to: 0.6; duration: 650; easing.type: Easing.InOutSine }
    NumberAnimation { to: 1; duration: 650; easing.type: Easing.InOutSine }
  }
}
