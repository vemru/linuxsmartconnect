import QtQuick 6.11
import QtQuick.Layouts
import qs.Commons
import qs.Widgets
import "./Services"

ColumnLayout {
  id: root
  property var pluginApi: null
  property bool hideWhenDisconnected: pluginApi?.pluginSettings?.hideWhenDisconnected ?? false
  property string mirrorBinary: pluginApi?.pluginSettings?.mirrorBinary ?? SmartConnect.defaultMirrorBinary
  property string displayBinary: pluginApi?.pluginSettings?.displayBinary ?? SmartConnect.defaultDisplayBinary
  property string filesBinary: pluginApi?.pluginSettings?.filesBinary ?? SmartConnect.defaultFilesBinary
  property string filesDir: pluginApi?.pluginSettings?.filesDir ?? SmartConnect.defaultFilesDir

  // Phone as display: kept in display.json (shared with the GTK app and smartconnect-display),
  // written on Apply.
  property string displayMode: SmartConnect.displayConfig.mode
  property string displaySize: SmartConnect.displayConfig.size
  property int displayScalePct: Math.round(SmartConnect.displayConfig.displayScale * 100)
  property int displayOskHeight: SmartConnect.displayConfig.oskHeight
  property bool displayAudio: SmartConnect.displayConfig.audio
  property bool displayFollowRotation: SmartConnect.displayConfig.followRotation

  spacing: Style.marginL

  Component.onCompleted: {
    SmartConnect.refreshDisplayConfig();
    SmartConnect.loadMuted();
  }

  NToggle {
    label: "Hide bar icon while disconnected"
    description: "Only show the phone in the bar while it is connected"
    checked: root.hideWhenDisconnected
    onToggled: checked => root.hideWhenDisconnected = checked
  }

  NTextInput {
    Layout.fillWidth: true
    label: "Screen mirroring viewer"
    description: "smartconnect-mirror, looked up on PATH, or a full path to it (~ is expanded)"
    text: root.mirrorBinary
    onTextChanged: root.mirrorBinary = text
  }

  NHeader {
    label: "Phone as display"
    description: "Show this PC's screen on the phone. Shared with the GTK app and used when the phone starts it."
  }

  NComboBox {
    Layout.fillWidth: true
    label: "Mode"
    description: "Extend: the phone is a second monitor. Duplicate: this PC's screen on the phone."
    model: [
      { key: "extend", name: "Extend" },
      { key: "mirror", name: "Duplicate" }
    ]
    currentKey: root.displayMode
    onSelected: key => root.displayMode = key
  }

  NComboBox {
    Layout.fillWidth: true
    label: "Resolution"
    description: "Native fills the phone's screen; default is what the phone asks for (usually 1920×1080). A custom size can be set in the GTK app."
    model: {
      const m = [
        { key: "native", name: "Phone native" },
        { key: "requested", name: "Phone default" }
      ];
      if (/^\d+x\d+$/.test(root.displaySize)) m.push({ key: root.displaySize, name: "Custom " + root.displaySize.replace("x", "×") });
      return m;
    }
    currentKey: root.displaySize
    onSelected: key => root.displaySize = key
  }

  NComboBox {
    Layout.fillWidth: true
    enabled: root.displayMode === "extend"
    label: "Display scale"
    description: "Extend: size of text and windows on the phone; bigger means less room"
    model: [100, 125, 150, 175, 200, 250, 300].map(p => ({ key: String(p), name: p + " %" }))
    currentKey: String(root.displayScalePct)
    onSelected: key => root.displayScalePct = parseInt(key, 10)
  }

  NSpinBox {
    label: "On-screen keyboard height"
    description: "In % of the shared screen, for the keyboard button on the phone"
    from: 15
    to: 80
    stepSize: 5
    suffix: " %"
    value: root.displayOskHeight
    onValueChanged: if (value !== root.displayOskHeight) root.displayOskHeight = value
  }

  NToggle {
    label: "Send PC audio to the phone"
    description: "Experimental"
    checked: root.displayAudio
    onToggled: checked => root.displayAudio = checked
  }

  NToggle {
    enabled: root.displayMode === "extend"
    label: "Rotate with the phone"
    description: "Experimental · Extend: turns the phone's monitor when you turn the phone"
    checked: root.displayFollowRotation
    onToggled: checked => root.displayFollowRotation = checked
  }

  NTextInput {
    Layout.fillWidth: true
    label: "Phone-as-display helper"
    description: "smartconnect-display, looked up on PATH, or a full path to it (~ is expanded)"
    text: root.displayBinary
    onTextChanged: root.displayBinary = text
  }

  NHeader {
    label: "Phone files"
    description: "Mount the phone's storage as a read-only folder (from the panel's Phone files tile)."
  }

  NTextInput {
    Layout.fillWidth: true
    label: "Mount folder"
    description: "Where the phone's storage is mounted (~ is expanded); defaults to ~/Phone, like the GTK app."
    text: root.filesDir
    onTextChanged: root.filesDir = text
  }

  NTextInput {
    Layout.fillWidth: true
    label: "Phone files helper"
    description: "smartconnect-files, looked up on PATH, or a full path to it (~ is expanded)"
    text: root.filesBinary
    onTextChanged: root.filesBinary = text
  }

  NHeader {
    label: "Muted apps"
    description: "Phone apps whose notifications don't pop up on this PC. Mute one with the bell next to its notification in the panel."
  }

  NText {
    visible: SmartConnect.mutedApps.length === 0
    text: SmartConnect.commandAvailable ? "No muted apps" : "smartconnectd is not running"
    color: Color.mOnSurfaceVariant
  }

  Repeater {
    model: SmartConnect.mutedApps
    delegate: RowLayout {
      id: mutedRow
      required property string modelData
      Layout.fillWidth: true
      spacing: Style.marginM
      ColumnLayout {
        Layout.fillWidth: true
        spacing: 0
        NText {
          text: SmartConnect.appName(mutedRow.modelData)
          color: Color.mOnSurface
          elide: Text.ElideRight
          Layout.fillWidth: true
        }
        NText {
          visible: SmartConnect.appName(mutedRow.modelData) !== mutedRow.modelData
          text: mutedRow.modelData
          color: Color.mOnSurfaceVariant
          pointSize: Style.fontSizeS
          elide: Text.ElideRight
          Layout.fillWidth: true
        }
      }
      NButton {
        text: "Unmute"
        outlined: true
        onClicked: SmartConnect.setAppMuted(mutedRow.modelData, false)
      }
    }
  }

  function saveSettings() {
    if (!pluginApi) return;
    pluginApi.pluginSettings.hideWhenDisconnected = root.hideWhenDisconnected;
    pluginApi.pluginSettings.mirrorBinary = root.mirrorBinary;
    pluginApi.pluginSettings.displayBinary = root.displayBinary;
    pluginApi.pluginSettings.filesBinary = root.filesBinary;
    pluginApi.pluginSettings.filesDir = root.filesDir;
    pluginApi.saveSettings();
    const c = SmartConnect.displayConfig;
    const changes = {
      mode: root.displayMode,
      size: root.displaySize,
      displayScale: root.displayScalePct / 100,
      oskHeight: root.displayOskHeight,
      audio: root.displayAudio,
      followRotation: root.displayFollowRotation
    };
    if (Object.keys(changes).some(k => changes[k] !== c[k])) SmartConnect.saveDisplayConfig(changes);
  }
}
