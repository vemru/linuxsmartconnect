import QtQuick 6.11
import QtQuick.Layouts
import qs.Commons
import qs.Services.UI
import qs.Widgets
import "./Services"

Item {
  id: root

  property var pluginApi: null
  readonly property var geometryPlaceholder: panelContainer
  property real contentPreferredWidth: 420 * Style.uiScaleRatio
  // Follows the content (notifications up to a cap), so a quiet phone gets a compact panel.
  property real contentPreferredHeight: Math.min(860 * Style.uiScaleRatio, mainColumn.implicitHeight + 2 * Style.marginL)
  readonly property bool allowAttach: true

  anchors.fill: parent

  property string crossEdge: pluginApi?.pluginSettings?.crossEdge ?? "right"
  readonly property string displayBinary: pluginApi?.pluginSettings?.displayBinary ?? ""
  readonly property string filesBinary: pluginApi?.pluginSettings?.filesBinary ?? ""
  readonly property string filesDir: pluginApi?.pluginSettings?.filesDir ?? ""
  readonly property string webcamBinary: pluginApi?.pluginSettings?.webcamBinary ?? ""
  // User-facing folder (keeps a leading ~); blank settings fall back to ~/Phone.
  readonly property string filesDirLabel: (filesDir || "").trim() || "~/Phone"

  // The panel exists only while open: keep the phone-display state fresh meanwhile.
  Component.onCompleted: {
    SmartConnect.displayWatch = true;
    SmartConnect.refreshDisplayConfig();
    SmartConnect.loadMuted();
    SmartConnect.probeWebcam();
    // Adopt a live mount at the files folder, or clear a stale one, before showing the tile.
    SmartConnect.syncFiles(root.filesBinary, root.filesDirLabel);
  }
  Component.onDestruction: SmartConnect.displayWatch = false

  function signalText(level) {
    if (level === undefined || level === null) return "—";
    return ["None", "Very weak", "Weak", "Fair", "Good", "Excellent"][Math.max(0, Math.min(5, level))];
  }

  function capitalized(s) {
    return s ? s.charAt(0).toUpperCase() + s.slice(1) : "";
  }

  function cycleCrossEdge() {
    const edges = SmartConnect.crossEdges;
    root.crossEdge = edges[(edges.indexOf(root.crossEdge) + 1) % edges.length];
    if (root.pluginApi?.pluginSettings) {
      root.pluginApi.pluginSettings.crossEdge = root.crossEdge;
      root.pluginApi.saveSettings();
    }
  }

  readonly property bool displayMirror: SmartConnect.displayConfig.mode === "mirror"
  readonly property string displayOption: displayMirror
    ? "Duplicate"
    : "Extend · " + Math.round((SmartConnect.displayConfig.displayScale ?? 2) * 100) + " %"

  // Quick-settings style tile: an icon and a title, plus an optional option chip ("Right edge")
  // that changes a setting without starting the feature. `available` (not `enabled`) gates the
  // tile, so the chip still works while the phone is disconnected.
  component ActionTile: Rectangle {
    id: tile

    property string icon: ""
    property string title: ""
    property string option: ""
    property string optionIcon: ""
    property string tooltipText: ""
    property string optionTooltip: ""
    property bool active: false
    property bool available: true
    property bool optionAvailable: true
    signal activated
    signal optionActivated

    readonly property bool hovered: tileMouse.containsMouse && available
    readonly property color fg: active ? Color.mOnPrimary : (hovered ? Color.mOnHover : Color.mOnSurface)

    implicitHeight: tileContent.implicitHeight + 2 * Style.marginM
    radius: Style.radiusM
    color: active ? Color.mPrimary : (hovered ? Color.mHover : Color.mSurface)
    border.width: active ? 0 : Style.borderS
    border.color: Color.mOutline
    opacity: available || active ? 1.0 : 0.5

    Behavior on color {
      ColorAnimation { duration: Style.animationFast }
    }

    MouseArea {
      id: tileMouse
      anchors.fill: parent
      hoverEnabled: true
      cursorShape: tile.available ? Qt.PointingHandCursor : Qt.ArrowCursor
      onClicked: if (tile.available) tile.activated()
      onEntered: if (tile.tooltipText) TooltipService.show(tile, tile.tooltipText, "auto")
      onExited: if (tile.tooltipText) TooltipService.hide(tile)
    }

    ColumnLayout {
      id: tileContent
      anchors.left: parent.left
      anchors.right: parent.right
      anchors.verticalCenter: parent.verticalCenter
      anchors.leftMargin: Style.marginM
      anchors.rightMargin: Style.marginM
      spacing: Style.marginS

      RowLayout {
        Layout.fillWidth: true
        spacing: Style.marginS
        NIcon {
          icon: tile.icon
          pointSize: Style.fontSizeL
          color: tile.fg
        }
        NText {
          text: tile.title
          color: tile.fg
          font.weight: Style.fontWeightSemiBold
          elide: Text.ElideRight
          Layout.fillWidth: true
        }
      }

      // The option chip sits above the tile's MouseArea, so clicking it doesn't toggle the tile.
      Rectangle {
        id: chip
        visible: tile.option !== ""
        Layout.alignment: Qt.AlignLeft
        Layout.maximumWidth: tileContent.width
        implicitWidth: chipRow.implicitWidth + 2 * Style.marginS
        implicitHeight: chipRow.implicitHeight + Style.marginXS * 2
        radius: height / 2
        color: chipMouse.containsMouse && tile.optionAvailable
          ? (tile.active ? Qt.rgba(1, 1, 1, 0.18) : Color.mHover)
          : "transparent"
        border.width: Style.borderS
        border.color: tile.active ? Color.mOnPrimary : Color.mOutline
        opacity: tile.optionAvailable ? 1.0 : 0.6

        RowLayout {
          id: chipRow
          anchors.centerIn: parent
          width: Math.min(implicitWidth, chip.Layout.maximumWidth - 2 * Style.marginS)
          spacing: Style.marginXS
          NIcon {
            visible: tile.optionIcon !== ""
            icon: tile.optionIcon
            pointSize: Style.fontSizeS
            color: chipMouse.containsMouse && tile.optionAvailable && !tile.active ? Color.mOnHover : tile.fg
          }
          NText {
            text: tile.option
            pointSize: Style.fontSizeXS
            color: chipMouse.containsMouse && tile.optionAvailable && !tile.active ? Color.mOnHover : tile.fg
            elide: Text.ElideRight
            Layout.fillWidth: true
          }
        }

        MouseArea {
          id: chipMouse
          anchors.fill: parent
          hoverEnabled: true
          cursorShape: tile.optionAvailable ? Qt.PointingHandCursor : Qt.ArrowCursor
          onClicked: if (tile.optionAvailable) tile.optionActivated()
          onEntered: if (tile.optionTooltip) TooltipService.show(chip, tile.optionTooltip, "auto")
          onExited: if (tile.optionTooltip) TooltipService.hide(chip)
        }
      }
    }
  }

  Rectangle {
    id: panelContainer
    anchors.fill: parent
    color: "transparent"

    ColumnLayout {
      id: mainColumn
      anchors.fill: parent
      anchors.margins: Style.marginL
      spacing: Style.marginM

      // Header
      RowLayout {
        Layout.fillWidth: true
        spacing: Style.marginM
        NIcon { icon: "device-mobile"; pointSize: Style.fontSizeXXL; color: Color.mPrimary }
        NText {
          text: "Smart Connect"
          pointSize: Style.fontSizeL
          font.weight: Style.fontWeightBold
          color: Color.mOnSurface
          Layout.fillWidth: true
        }
        NIconButton {
          icon: "qrcode"
          tooltipText: SmartConnect.showPairing ? "Back" : "Pair a new phone"
          onClicked: {
            if (SmartConnect.showPairing) SmartConnect.showPairing = false;
            else SmartConnect.pairNewPhone();
          }
        }
      }

      // Daemon not running
      NBox {
        visible: !SmartConnect.daemonAvailable
        Layout.fillWidth: true
        implicitHeight: downCol.implicitHeight + Style.marginXL
        ColumnLayout {
          id: downCol
          anchors.fill: parent
          anchors.margins: Style.marginM
          NText { text: "smartconnectd is not running"; font.weight: Style.fontWeightBold; color: Color.mError }
          NText {
            text: "Start the daemon, then this panel connects automatically."
            color: Color.mOnSurfaceVariant
            wrapMode: Text.Wrap
            Layout.fillWidth: true
          }
        }
      }

      // Pairing view
      NBox {
        visible: SmartConnect.daemonAvailable && SmartConnect.showPairing
        Layout.fillWidth: true
        Layout.preferredHeight: 400 * Style.uiScaleRatio
        ColumnLayout {
          anchors.fill: parent
          anchors.margins: Style.marginM
          spacing: Style.marginM
          NText {
            text: "Scan with Smart Connect on the phone (+ → Scan QR code)"
            color: Color.mOnSurface
            wrapMode: Text.Wrap
            Layout.fillWidth: true
          }
          Rectangle {
            color: "white"
            radius: Style.radiusM
            Layout.alignment: Qt.AlignHCenter
            Layout.preferredWidth: Math.min(parent.width, 300 * Style.uiScaleRatio)
            Layout.preferredHeight: Layout.preferredWidth
            Image {
              anchors.fill: parent
              anchors.margins: Style.marginS
              source: SmartConnect.pairingQr ? SmartConnect.pairingQr + "?" + Date.now() : ""
              cache: false
              fillMode: Image.PreserveAspectFit
              sourceSize.width: 600
              sourceSize.height: 600
            }
          }
        }
      }

      // Phone summary: wallpaper thumbnail, name, connection state and stats.
      NBox {
        visible: SmartConnect.daemonAvailable && !SmartConnect.showPairing
        Layout.fillWidth: true
        implicitHeight: phoneRow.implicitHeight + 2 * Style.marginM

        RowLayout {
          id: phoneRow
          anchors.left: parent.left
          anchors.right: parent.right
          anchors.verticalCenter: parent.verticalCenter
          anchors.margins: Style.marginM
          spacing: Style.marginM

          Rectangle {
            Layout.preferredWidth: 54 * Style.uiScaleRatio
            Layout.preferredHeight: 108 * Style.uiScaleRatio
            Layout.alignment: Qt.AlignTop
            radius: 10 * Style.uiScaleRatio
            color: "#1c1c1e"
            NImageRounded {
              anchors.fill: parent
              anchors.margins: 3
              radius: 8 * Style.uiScaleRatio
              imagePath: SmartConnect.wallpaper
              fallbackIcon: "device-mobile"
              opacity: SmartConnect.connected ? 1.0 : 0.45
            }
          }

          ColumnLayout {
            Layout.fillWidth: true
            Layout.alignment: Qt.AlignTop
            spacing: Style.marginXS

            NText {
              text: SmartConnect.paired ? SmartConnect.phoneName : "No phone paired"
              pointSize: Style.fontSizeL
              font.weight: Style.fontWeightBold
              color: Color.mOnSurface
              wrapMode: Text.Wrap
              maximumLineCount: 2
              elide: Text.ElideRight
              Layout.fillWidth: true
            }

            RowLayout {
              spacing: Style.marginS
              Rectangle {
                implicitWidth: 8 * Style.uiScaleRatio
                implicitHeight: implicitWidth
                radius: implicitWidth / 2
                color: SmartConnect.connected ? Color.mPrimary : Color.mOutline
              }
              NText {
                text: SmartConnect.connected ? "Connected over Wi-Fi" : (SmartConnect.paired ? "Disconnected" : "Use the QR button to pair")
                color: SmartConnect.connected ? Color.mPrimary : Color.mOnSurfaceVariant
              }
            }

            Flow {
              visible: SmartConnect.connected
              Layout.fillWidth: true
              Layout.topMargin: Style.marginXS
              spacing: Style.marginS

              Repeater {
                model: [
                  { icon: SmartConnect.batteryIcon(), text: SmartConnect.battery >= 0 ? SmartConnect.battery + "%" + (SmartConnect.charging ? " ⚡" : "") : "—", show: true, tip: "Battery" },
                  { icon: "wifi", text: SmartConnect.status.wifi ? root.signalText(SmartConnect.status.wifiSignal) : "Off", show: true, tip: "Phone Wi-Fi" },
                  { icon: "antenna-bars-5", text: root.signalText(SmartConnect.status.cellSignal), show: true, tip: "Mobile signal" },
                  { icon: "message", text: String(SmartConnect.status.unread ?? 0), show: (SmartConnect.status.unread ?? -1) >= 0, tip: "Unread messages" }
                ]
                delegate: Rectangle {
                  required property var modelData
                  visible: modelData.show
                  implicitWidth: statRow.implicitWidth + 2 * Style.marginS
                  implicitHeight: statRow.implicitHeight + Style.marginXS * 2
                  radius: height / 2
                  color: Color.mSurface
                  border.width: Style.borderS
                  border.color: Color.mOutline
                  RowLayout {
                    id: statRow
                    anchors.centerIn: parent
                    spacing: Style.marginXS
                    NIcon { icon: modelData.icon; pointSize: Style.fontSizeS; color: Color.mOnSurface }
                    NText { text: modelData.text; pointSize: Style.fontSizeXS; color: Color.mOnSurface }
                  }
                  MouseArea {
                    anchors.fill: parent
                    hoverEnabled: true
                    onEntered: TooltipService.show(parent, modelData.tip, "auto")
                    onExited: TooltipService.hide(parent)
                  }
                }
              }
            }
          }
        }
      }

      // Actions: 2×2 quick tiles.
      GridLayout {
        id: actionsGrid
        visible: SmartConnect.daemonAvailable && !SmartConnect.showPairing && SmartConnect.paired
        Layout.fillWidth: true
        columns: 2
        rowSpacing: Style.marginS
        columnSpacing: Style.marginS

        readonly property real tileWidth: (width - columnSpacing) / 2

        ActionTile {
          Layout.fillWidth: true
          Layout.preferredWidth: actionsGrid.tileWidth
          Layout.fillHeight: true
          icon: SmartConnect.mirroring ? "screen-share-off" : "screen-share"
          title: SmartConnect.mirroring ? "Stop mirroring" : "Mirror screen"
          option: "Phone → this PC"
          optionAvailable: false
          active: SmartConnect.mirroring
          available: SmartConnect.connected || SmartConnect.mirroring
          tooltipText: SmartConnect.mirroring ? "Close the phone-screen window" : "Show the phone's screen in a window here"
          onActivated: SmartConnect.toggleMirror(root.pluginApi?.pluginSettings?.mirrorBinary ?? "")
        }

        ActionTile {
          Layout.fillWidth: true
          Layout.preferredWidth: actionsGrid.tileWidth
          Layout.fillHeight: true
          icon: SmartConnect.crossControlling ? "mouse-off" : "mouse"
          title: SmartConnect.crossControlling ? "Stop Cross Control" : "Cross Control"
          option: root.capitalized(root.crossEdge) + " edge"
          optionIcon: ({ right: "arrow-bar-right", left: "arrow-bar-left", top: "arrow-bar-up", bottom: "arrow-bar-down" })[root.crossEdge] || "arrow-bar-right"
          optionAvailable: !SmartConnect.crossControlling
          optionTooltip: "Screen edge that leads to the phone (click to change)"
          active: SmartConnect.crossControlling
          available: SmartConnect.connected || SmartConnect.crossControlling
          tooltipText: SmartConnect.crossControlling ? "Give the mouse and keyboard back to this PC"
                                                     : "Move the mouse past the " + root.crossEdge + " edge to control the phone"
          onActivated: SmartConnect.toggleCross(root.crossEdge)
          onOptionActivated: root.cycleCrossEdge()
        }

        ActionTile {
          Layout.fillWidth: true
          Layout.preferredWidth: actionsGrid.tileWidth
          Layout.fillHeight: true
          icon: SmartConnect.displayRunning ? "cast-off" : "cast"
          title: SmartConnect.displayRunning ? "Stop phone display" : "Phone as display"
          option: root.displayOption
          optionIcon: root.displayMirror ? "copy" : "columns-2"
          optionAvailable: !SmartConnect.displayRunning && SmartConnect.displayBusy === ""
          optionTooltip: root.displayMirror ? "Duplicate: this PC's screen on the phone. Click for Extend (more options in Settings)."
                                            : "Extend: the phone is a second monitor. Click for Duplicate (more options in Settings)."
          active: SmartConnect.displayRunning
          available: SmartConnect.displayBusy === "" && !SmartConnect.displayRestart
                     && (SmartConnect.connected || SmartConnect.displayRunning)
          tooltipText: SmartConnect.displayRunning ? "Stop showing this PC's screen on the phone"
                     : (root.displayMirror ? "Show this PC's screen on the phone" : "Use the phone as a second monitor")
          onActivated: SmartConnect.displayCommand("toggle", root.displayBinary)
          onOptionActivated: SmartConnect.saveDisplayConfig({ mode: root.displayMirror ? "extend" : "mirror" })
        }

        ActionTile {
          Layout.fillWidth: true
          Layout.preferredWidth: actionsGrid.tileWidth
          Layout.fillHeight: true
          icon: SmartConnect.webcamRunning ? "video-off" : "video"
          title: SmartConnect.webcamRunning ? "Stop webcam" : "Webcam"
          option: SmartConnect.webcamRunning ? "Flip camera" : "Phone camera"
          optionIcon: SmartConnect.webcamRunning ? "switch-horizontal" : "camera"
          optionAvailable: SmartConnect.webcamRunning
          optionTooltip: "Switch front and back camera"
          active: SmartConnect.webcamRunning
          available: SmartConnect.connected || SmartConnect.webcamRunning
          tooltipText: SmartConnect.webcamRunning ? "Stop the phone webcam stream" : "Use the phone camera as a PC webcam"
          onActivated: SmartConnect.toggleWebcam(root.webcamBinary)
          onOptionActivated: SmartConnect.switchWebcam(root.webcamBinary)
        }

        ActionTile {
          Layout.fillWidth: true
          Layout.preferredWidth: actionsGrid.tileWidth
          Layout.fillHeight: true
          icon: SmartConnect.filesMounted ? "folder-open" : "folder"
          title: SmartConnect.filesMounted ? "Unmount files" : "Phone files"
          option: SmartConnect.filesMounted ? "Open"
                 : (SmartConnect.filesMounting ? "Mounting…" : root.filesDirLabel)
          optionIcon: SmartConnect.filesMounted ? "external-link" : "folder"
          optionAvailable: SmartConnect.filesMounted
          optionTooltip: SmartConnect.filesMounted ? "Open the mounted folder in your file manager" : ""
          active: SmartConnect.filesMounted
          available: SmartConnect.connected || SmartConnect.filesMounting || SmartConnect.filesMounted
          tooltipText: SmartConnect.filesMounted ? "Unmount the phone's storage (" + root.filesDirLabel + ")"
                     : (SmartConnect.filesMounting ? "Mounting the phone's storage…"
                       : "Mount the phone's storage as a read-only folder at " + root.filesDirLabel)
          onActivated: SmartConnect.toggleFiles(root.filesBinary, root.filesDirLabel)
          onOptionActivated: SmartConnect.openFiles(root.filesBinary, root.filesDirLabel)
        }

        ActionTile {
          Layout.fillWidth: true
          Layout.preferredWidth: actionsGrid.tileWidth
          Layout.fillHeight: true
          icon: SmartConnect.connected ? "plug-connected-x" : "plug-connected"
          title: SmartConnect.busy ? "Working…" : (SmartConnect.connected ? "Disconnect" : "Connect")
          option: SmartConnect.connected ? "Wi-Fi" : "Wi-Fi or Bluetooth"
          optionAvailable: false
          available: !SmartConnect.busy
          tooltipText: SmartConnect.connected ? "Disconnect the phone (it stays paired)" : "Reconnect to the phone"
          onActivated: SmartConnect.connected ? SmartConnect.disconnectPhone() : SmartConnect.connectPhone()
        }
      }

      // Status and errors from the helpers, one line each.
      ColumnLayout {
        visible: SmartConnect.daemonAvailable && !SmartConnect.showPairing
        Layout.fillWidth: true
        spacing: Style.marginXS

        Repeater {
          model: [
            { text: SmartConnect.lastMessage, error: false },
            { text: SmartConnect.mirrorStatus, error: true },
            { text: SmartConnect.crossStatus, error: true },
            { text: SmartConnect.webcamStatus, error: true },
            { text: SmartConnect.filesStatus, error: SmartConnect.filesStatusError },
            { text: SmartConnect.paired ? SmartConnect.displayStatus : "", error: SmartConnect.displayStatusIsError }
          ]
          delegate: NText {
            required property var modelData
            visible: modelData.text !== ""
            text: modelData.text
            color: modelData.error ? Color.mError : Color.mOnSurfaceVariant
            pointSize: Style.fontSizeS
            wrapMode: Text.Wrap
            maximumLineCount: 2
            elide: Text.ElideRight
            Layout.fillWidth: true
          }
        }
      }

      // Recent phone notifications, with inline reply for apps that accept replies
      NText {
        visible: notifBox.visible
        text: "Notifications"
        font.weight: Style.fontWeightSemiBold
        color: Color.mOnSurfaceVariant
        Layout.topMargin: Style.marginXS
      }

      NBox {
        id: notifBox
        visible: SmartConnect.daemonAvailable && !SmartConnect.showPairing && SmartConnect.notifications.length > 0
        Layout.fillWidth: true
        Layout.fillHeight: true
        Layout.preferredHeight: Math.min(notifList.contentHeight + 2 * Style.marginM, 440 * Style.uiScaleRatio)

        ListView {
          id: notifList
          anchors.fill: parent
          anchors.margins: Style.marginM
          clip: true
          spacing: Style.marginM
          model: SmartConnect.notifications

          delegate: ColumnLayout {
            id: item
            required property var modelData
            width: notifList.width
            spacing: Style.marginXS

            RowLayout {
              Layout.fillWidth: true
              spacing: Style.marginS
              NText {
                text: (item.modelData.app ? item.modelData.app + " · " : "") + item.modelData.title
                font.weight: Style.fontWeightBold
                color: Color.mOnSurface
                elide: Text.ElideRight
                Layout.fillWidth: true
              }
              // Per-app mute of the desktop popups; shown pressed while the app is muted.
              NIconButton {
                readonly property bool muted: SmartConnect.isMuted(item.modelData.pkg ?? "")
                readonly property string appLabel: item.modelData.app || "this app"
                visible: !!item.modelData.pkg
                baseSize: Style.baseWidgetSize * 0.75
                icon: "bell-off"
                colorBg: muted ? Color.mPrimary : Color.smartAlpha(Color.mSurfaceVariant)
                colorFg: muted ? Color.mOnPrimary : Color.mPrimary
                tooltipText: muted ? appLabel + " is muted: no desktop popups. Click to unmute"
                                   : "Mute " + appLabel + ": no desktop popups from it"
                onClicked: SmartConnect.setAppMuted(item.modelData.pkg, !muted)
              }
            }
            NText {
              text: modelData.body
              color: Color.mOnSurfaceVariant
              wrapMode: Text.Wrap
              maximumLineCount: 4
              elide: Text.ElideRight
              Layout.fillWidth: true
            }
            RowLayout {
              visible: modelData.replyable
              Layout.fillWidth: true
              spacing: Style.marginS
              NTextInput {
                id: replyInput
                Layout.fillWidth: true
                placeholderText: "Reply…"
                showClearButton: false
                Component.onCompleted: if (SmartConnect.replyKey === item.modelData.key) inputItem.forceActiveFocus()
                Connections {
                  target: SmartConnect
                  function onReplyKeyChanged() {
                    if (SmartConnect.replyKey === item.modelData.key) {
                      notifList.positionViewAtIndex(notifList.indexAt(0, item.y), ListView.Contain);
                      replyInput.inputItem.forceActiveFocus();
                    }
                  }
                }
                Connections {
                  target: replyInput.inputItem
                  function onAccepted() { sendButton.clicked(); }
                }
              }
              NIconButton {
                id: sendButton
                icon: "send"
                tooltipText: "Send reply"
                onClicked: {
                  SmartConnect.reply(item.modelData.key, replyInput.text);
                  replyInput.text = "";
                }
              }
            }
          }
        }
      }

      // Keeps the content top-aligned when the panel is taller than it.
      Item {
        visible: !notifBox.visible
        Layout.fillHeight: true
      }
    }
  }
}
