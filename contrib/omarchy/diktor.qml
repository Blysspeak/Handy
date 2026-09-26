// Diktor voice mode in the Omarchy bar (Hyprland). Install as
// ~/.config/omarchy/bar/modules/diktor.qml and add { "id": "diktor", "type": "qml" }
// to bar.layout in ~/.config/omarchy/shell.json.
//
// State comes from $XDG_RUNTIME_DIR/diktor/voice-state.json, which Diktor
// replaces atomically (voice_session/bar_state.rs):
//   {state, tooltip, rescue: null | {id, count, title, reason, text, copy, close}}
// The icon is only the live voice state. Left click toggles voice mode, right
// click copies text that was never inserted. While `rescue` is set, a popup
// shows that text with Copy / Close; the phrases stay in history.db either way.
import QtQuick
import QtQuick.Layouts
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
  property var rescue: null
  // Hidden locally when another bar popup takes over; a new failure (new id) reopens it.
  property string hiddenRescueId: ""
  readonly property string handy: "\"$HOME/.local/bin/handy\""

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function apply(raw) {
    var data = {}
    try { data = JSON.parse(raw) } catch (e) {}
    voiceState = String(data.state || "off")
    tooltip = String(data.tooltip || "")
    rescue = data.rescue && data.rescue.text ? data.rescue : null
  }

  // Called by the bar's one-popup-at-a-time coordinator.
  function close() {
    if (rescue) hiddenRescueId = rescue.id
  }

  function run(flag) {
    if (root.bar) root.bar.run(root.handy + " " + flag)
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
    active: ["speech", "sending", "error"].indexOf(root.voiceState) >= 0
    activeColor: root.voiceState === "error" ? Color.urgent : Color.accent
    tooltipText: root.tooltip ? "Diktor\n" + root.tooltip : "Diktor"
    onPressed: function(b) {
      if (b === Qt.RightButton) root.run("--copy-voice-text")
      else if (b === Qt.LeftButton) root.run("--toggle-voice-mode")
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

  // Text that could not be inserted. "hover" mode: no focus grab, so the
  // popup never takes keyboard focus from the window voice mode types into.
  PopupCard {
    id: popup
    anchorItem: button
    bar: root.bar
    owner: root
    triggerMode: "hover"
    open: root.rescue !== null && root.rescue.id !== root.hiddenRescueId
    contentWidth: Style.space(380)
    contentHeight: fittedContentHeight(body.implicitHeight, Style.space(360))

    ColumnLayout {
      id: body
      anchors.fill: parent
      spacing: Style.space(8)

      Text {
        Layout.fillWidth: true
        text: root.rescue ? root.rescue.title : ""
        color: Color.urgent
        font.family: Style.font.family
        font.pixelSize: Style.font.title
        font.bold: true
      }

      Text {
        Layout.fillWidth: true
        text: root.rescue ? root.rescue.reason : ""
        color: Color.popups.text
        opacity: 0.7
        wrapMode: Text.Wrap
        font.family: Style.font.family
        font.pixelSize: Style.font.bodySmall
      }

      Flickable {
        id: scroller
        Layout.fillWidth: true
        Layout.fillHeight: true
        Layout.preferredHeight: Math.min(phrase.implicitHeight, Style.space(200))
        contentHeight: phrase.implicitHeight
        clip: true
        boundsBehavior: Flickable.StopAtBounds

        Text {
          id: phrase
          width: scroller.width
          text: root.rescue ? root.rescue.text : ""
          color: Color.popups.text
          wrapMode: Text.Wrap
          textFormat: Text.PlainText
          font.family: Style.font.family
          font.pixelSize: Style.font.body
        }
      }

      RowLayout {
        Layout.alignment: Qt.AlignRight
        spacing: Style.space(8)

        Button {
          text: root.rescue ? root.rescue.close : ""
          bordered: true
          onClicked: {
            root.close()
            root.run("--dismiss-voice-rescue")
          }
        }

        Button {
          text: root.rescue ? root.rescue.copy : ""
          bordered: true
          onClicked: {
            root.close()
            root.run("--copy-voice-text")
          }
        }
      }
    }
  }
}
