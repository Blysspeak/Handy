// Diktor in the Omarchy bar (Hyprland). Install as
// ~/.config/omarchy/bar/modules/diktor.qml and add { "id": "diktor", "type": "qml" }
// to bar.layout in ~/.config/omarchy/shell.json.
//
// State comes from $XDG_RUNTIME_DIR/diktor/voice-state.json, which the diktor
// daemon replaces atomically (src/voice_session/bar_state.rs):
//   {state, tooltip, language, rescue: null | {id, count, title, reason, text, copy, close}}
// The icon is only the live voice state. Left click toggles voice mode
// (`diktor toggle`), right click copies text that was never inserted
// (`diktor copy`), middle click opens the Diktor panel (`diktor panel`, which
// flips $XDG_RUNTIME_DIR/diktor/panel). While `rescue` is set, a popup shows
// that text with Copy / Close on the focused monitor's bar.
//
// The panel reads everything through `diktor ... --json` and writes through
// the same CLI; nothing runs while it is closed.
import QtQuick
import QtQuick.Layouts
import Quickshell
import Quickshell.Hyprland
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
  property string language: ""
  property var rescue: null
  // Hidden locally when another bar popup takes over; a new failure (new id) reopens it.
  property string hiddenRescueId: ""
  readonly property string diktor: Quickshell.env("HOME") + "/.local/bin/diktor"

  // This bar's output against the one Hyprland has focused: popups show once,
  // on the monitor the owner is looking at.
  readonly property string screenName: {
    var window = button.QsWindow.window
    return window && window.screen ? String(window.screen.name || "") : ""
  }
  readonly property bool onFocusedMonitor: {
    var monitor = Hyprland.focusedMonitor
    return !monitor || screenName === "" || String(monitor.name) === screenName
  }

  // Panel state and data (loaded when it opens).
  property bool panelRequested: false
  readonly property bool panelOpen: panelRequested && onFocusedMonitor
  property var models: []
  property var glossary: []
  property var misses: []
  property var phrases: []
  property string downloadFile: ""
  property string downloadText: ""
  property string panelError: ""
  property int editingPhrase: -1

  implicitWidth: button.implicitWidth
  implicitHeight: button.implicitHeight

  function apply(raw) {
    var data = {}
    try { data = JSON.parse(raw) } catch (e) {}
    voiceState = String(data.state || "off")
    tooltip = String(data.tooltip || "")
    language = String(data.language || "")
    rescue = data.rescue && data.rescue.text ? data.rescue : null
  }

  // Called by the bar's one-popup-at-a-time coordinator.
  function close() {
    if (rescue) hiddenRescueId = rescue.id
    if (panelRequested) run(["panel", "close"])
  }

  // Fire-and-forget CLI call through the bar (argv, no shell).
  function run(args) {
    if (root.bar) root.bar.run(Util.shellQuote(root.diktor) + " " + args.map(function(a) { return Util.shellQuote(String(a)) }).join(" "))
  }

  // CLI call whose JSON reply lands in `handler`, then the panel reloads.
  function call(args, reload) {
    var proc = callComponent.createObject(root, { command: [root.diktor].concat(args), reloadAfter: reload === true })
    proc.running = true
  }

  function loadPanel() {
    modelsProc.running = true
    glossaryProc.running = true
    missesProc.running = true
    loadPhrases()
  }

  function loadPhrases() {
    var query = searchField.text.trim()
    phrasesProc.command = query
      ? [root.diktor, "phrases", "search", query, "--limit", "40", "--json"]
      : [root.diktor, "phrases", "recent", "--limit", "40", "--json"]
    phrasesProc.running = true
  }

  function parse(text, fallback) {
    try { return JSON.parse(String(text || "").trim()) } catch (e) { return fallback }
  }

  onPanelOpenChanged: if (panelOpen) loadPanel()

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

  FileView {
    id: panelFile
    path: Quickshell.env("XDG_RUNTIME_DIR") + "/diktor/panel"
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: root.panelRequested = String(text()).trim() === "open"
    onLoadFailed: {
      root.panelRequested = false
      retry.start()
    }
  }

  // A missing file is never watched, and the bar usually starts before
  // Diktor creates it: look again until it exists, then changes are pushed.
  Timer {
    id: retry
    interval: 1000
    onTriggered: {
      stateFile.reload()
      panelFile.reload()
    }
  }

  Component {
    id: callComponent
    Process {
      property bool reloadAfter: false
      stdout: StdioCollector {
        waitForEnd: true
        onStreamFinished: {
          var reply = root.parse(text, null)
          root.panelError = reply && reply.ok === false ? String(reply.error || "") : ""
        }
      }
      stderr: StdioCollector {
        waitForEnd: true
        onStreamFinished: if (String(text).trim()) root.panelError = String(text).trim()
      }
      onExited: {
        if (reloadAfter && root.panelOpen) root.loadPanel()
        destroy()
      }
    }
  }

  Process {
    id: modelsProc
    command: [root.diktor, "model", "list", "--json"]
    stdout: StdioCollector { waitForEnd: true; onStreamFinished: root.models = root.parse(text, []) }
  }
  Process {
    id: glossaryProc
    command: [root.diktor, "glossary", "list", "--json"]
    stdout: StdioCollector { waitForEnd: true; onStreamFinished: root.glossary = root.parse(text, []) }
  }
  Process {
    id: missesProc
    command: [root.diktor, "misses", "--json"]
    stdout: StdioCollector { waitForEnd: true; onStreamFinished: root.misses = root.parse(text, []).slice(0, 6) }
  }
  Process {
    id: phrasesProc
    stdout: StdioCollector { waitForEnd: true; onStreamFinished: root.phrases = root.parse(text, []) }
  }
  // A download reports one JSON line per update.
  Process {
    id: downloadProc
    stdout: SplitParser {
      onRead: function(line) {
        var progress = root.parse(line, {})
        if (progress.total) root.downloadText = Math.floor(100 * progress.bytes / progress.total) + "%"
        if (progress.ok) root.downloadText = "готово"
      }
    }
    stderr: StdioCollector { waitForEnd: true; onStreamFinished: if (String(text).trim()) root.panelError = String(text).trim() }
    onExited: if (root.panelOpen) modelsProc.running = true
  }

  BarIconButton {
    id: button
    anchors.fill: parent
    bar: root.bar
    text: "󰍬"
    dimmed: root.voiceState === "off"
    active: ["speech", "sending", "error"].indexOf(root.voiceState) >= 0
    activeColor: root.voiceState === "error" ? Color.urgent : Color.accent
    tooltipText: (root.language ? "Diktor · " + root.language.toUpperCase() : "Diktor")
      + (root.tooltip ? "\n" + root.tooltip : "")
    onPressed: function(b) {
      if (b === Qt.RightButton) root.run(["copy"])
      else if (b === Qt.MiddleButton) root.run(["panel", "toggle"])
      else if (b === Qt.LeftButton) root.run(["toggle"])
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
    open: root.onFocusedMonitor && !root.panelOpen && root.rescue !== null && root.rescue.id !== root.hiddenRescueId
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
            root.run(["dismiss"])
          }
        }

        Button {
          text: root.rescue ? root.rescue.copy : ""
          bordered: true
          onClicked: {
            root.close()
            root.run(["copy"])
          }
        }
      }
    }
  }

  // The Diktor panel: model, glossary, journal. Click mode: it takes
  // input (text fields), a click outside closes it.
  PopupCard {
    id: panel
    anchorItem: button
    bar: root.bar
    owner: root
    triggerMode: "click"
    open: root.panelOpen
    contentWidth: fittedContentWidth(Style.space(460))
    contentHeight: cappedContentHeight(Style.space(640))

    Flickable {
      id: panelScroll
      anchors.fill: parent
      contentHeight: sections.implicitHeight
      clip: true
      boundsBehavior: Flickable.StopAtBounds

      ColumnLayout {
        id: sections
        width: panelScroll.width
        spacing: Style.space(8)

        Text {
          Layout.fillWidth: true
          visible: root.panelError !== ""
          text: root.panelError
          color: Color.urgent
          wrapMode: Text.Wrap
          textFormat: Text.PlainText
          font.family: Style.font.family
          font.pixelSize: Style.font.bodySmall
        }

        // ---- Recognition
        PanelSectionHeader { text: "Распознавание" }

        Dropdown {
          Layout.fillWidth: true
          label: "Модель"
          options: root.models.filter(function(m) { return m.installed }).map(function(m) {
            return { value: m.file, label: m.name + (m.quant ? " " + m.quant : "") }
          })
          value: {
            var current = root.models.filter(function(m) { return m.current })
            return current.length ? current[0].file : ""
          }
          onChanged: function(value) { root.call(["model", "use", value, "--json"], true) }
        }

        RowLayout {
          Layout.fillWidth: true
          spacing: Style.space(6)
          Text {
            text: "Язык"
            color: Color.popups.text
            font.family: Style.font.family
            font.pixelSize: Style.font.body
          }
          Item { Layout.fillWidth: true }
          Repeater {
            model: ["auto", "ru", "en"]
            Button {
              required property string modelData
              text: modelData
              bordered: true
              selected: root.language === modelData
              onClicked: root.call(["lang", modelData], false)
            }
          }
        }

        RowLayout {
          Layout.fillWidth: true
          spacing: Style.space(6)
          Dropdown {
            id: catalogPick
            Layout.fillWidth: true
            label: "Скачать"
            options: root.models.filter(function(m) { return !m.installed && m.catalog }).map(function(m) {
              return { value: m.file, label: m.name + " " + m.quant + " · " + Math.round(m.size_bytes / 1e6) + " МБ" }
            })
            onChanged: function(value) { root.downloadFile = value }
          }
          Button {
            text: downloadProc.running ? root.downloadText || "…" : "Скачать"
            bordered: true
            enabled: !downloadProc.running && root.downloadFile !== ""
            onClicked: {
              root.downloadText = ""
              downloadProc.command = [root.diktor, "model", "download", root.downloadFile, "--json"]
              downloadProc.running = true
            }
          }
        }

        PanelSeparator { Layout.fillWidth: true }

        // ---- Glossary
        PanelSectionHeader { text: "Словарь" }

        RowLayout {
          Layout.fillWidth: true
          spacing: Style.space(6)
          TextField { id: spokenField; Layout.fillWidth: true; placeholderText: "как слышит" }
          TextField { id: writtenField; Layout.fillWidth: true; placeholderText: "как писать" }
          Button {
            id: stemToggle
            property bool checked: false
            text: "падежи"
            bordered: true
            selected: checked
            tooltipText: "имя: берёт падежи и приклеенный предлог (с хабом → с xhub'ом)"
            onClicked: checked = !checked
          }
          Button {
            text: "Добавить"
            bordered: true
            enabled: spokenField.text.trim() !== "" && writtenField.text.trim() !== ""
            onClicked: {
              var args = ["glossary", "add", spokenField.text.trim(), writtenField.text.trim(), "--json"]
              if (stemToggle.checked) args.push("--stem")
              root.call(args, true)
              spokenField.text = ""
              writtenField.text = ""
            }
          }
        }

        Repeater {
          model: root.misses
          RowLayout {
            required property var modelData
            Layout.fillWidth: true
            Text {
              Layout.fillWidth: true
              text: "не в словаре: " + modelData.token + " ×" + modelData.count
              color: Color.popups.text
              opacity: 0.7
              elide: Text.ElideRight
              font.family: Style.font.family
              font.pixelSize: Style.font.bodySmall
            }
            Button {
              text: "в поле"
              onClicked: spokenField.text = modelData.token.toLowerCase()
            }
          }
        }

        Repeater {
          model: root.glossary
          RowLayout {
            required property var modelData
            Layout.fillWidth: true
            spacing: Style.space(6)
            Text {
              Layout.fillWidth: true
              text: (modelData.kind === "stem" ? "~" : "") + modelData.from + " → " + modelData.to
                + (modelData.enabled ? "" : "  (ждёт)") + (modelData.source === "claude" ? "  · claude" : "")
              color: Color.popups.text
              elide: Text.ElideRight
              textFormat: Text.PlainText
              font.family: Style.font.family
              font.pixelSize: Style.font.body
            }
            Button {
              visible: !modelData.enabled
              text: "Принять"
              bordered: true
              onClicked: root.call(["glossary", "approve", modelData.from, "--json"], true)
            }
            Button {
              text: "Убрать"
              onClicked: {
                var args = ["glossary", "remove", modelData.from, "--json"]
                args.push(modelData.kind === "stem" ? "--stem" : "--word")
                root.call(args, true)
              }
            }
          }
        }

        PanelSeparator { Layout.fillWidth: true }

        // ---- Journal
        PanelSectionHeader { text: "Журнал" }

        TextField {
          id: searchField
          Layout.fillWidth: true
          placeholderText: "поиск по фразам"
          onTextChanged: searchDelay.restart()
        }
        Timer {
          id: searchDelay
          interval: 300
          onTriggered: if (root.panelOpen) root.loadPhrases()
        }

        Repeater {
          model: root.phrases
          ColumnLayout {
            required property var modelData
            Layout.fillWidth: true
            spacing: Style.space(2)
            Text {
              Layout.fillWidth: true
              text: (modelData.corrected || modelData.text)
              color: modelData.status === "delivered" ? Color.popups.text : Color.urgent
              wrapMode: Text.Wrap
              textFormat: Text.PlainText
              font.family: Style.font.family
              font.pixelSize: Style.font.body
            }
            Text {
              Layout.fillWidth: true
              visible: modelData.raw !== modelData.text
              text: "распознано: " + modelData.raw
              color: Color.popups.text
              opacity: 0.6
              wrapMode: Text.Wrap
              textFormat: Text.PlainText
              font.family: Style.font.family
              font.pixelSize: Style.font.bodySmall
            }
            RowLayout {
              Layout.fillWidth: true
              spacing: Style.space(6)
              TextField {
                id: correctionField
                Layout.fillWidth: true
                visible: root.editingPhrase === modelData.id
                text: modelData.corrected || modelData.text
              }
              Item { Layout.fillWidth: true; visible: root.editingPhrase !== modelData.id }
              Button {
                text: "Копировать"
                onClicked: root.call(["copy", "--id", modelData.id], false)
              }
              Button {
                text: root.editingPhrase === modelData.id ? "Сохранить" : "Исправить"
                onClicked: {
                  if (root.editingPhrase === modelData.id) {
                    root.call(["phrases", "correct", modelData.id, correctionField.text, "--json"], true)
                    root.editingPhrase = -1
                  } else {
                    root.editingPhrase = modelData.id
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}
