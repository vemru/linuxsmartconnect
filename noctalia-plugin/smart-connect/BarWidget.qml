import QtQuick 6.11
import Quickshell
import qs.Commons
import qs.Modules.Bar.Extras
import qs.Services.UI
import "./Services"

Item {
  id: root

  property var pluginApi: null
  property ShellScreen screen
  property string widgetId: ""
  property string section: ""
  property int sectionWidgetIndex: -1
  property int sectionWidgetsCount: 0

  readonly property bool hideWhenDisconnected: pluginApi?.pluginSettings?.hideWhenDisconnected ?? false
  readonly property bool shown: !hideWhenDisconnected || SmartConnect.connected

  implicitWidth: pill.width
  implicitHeight: pill.height
  visible: shown
  opacity: shown ? 1.0 : 0.0

  BarPill {
    id: pill
    screen: root.screen
    oppositeDirection: BarService.getPillDirection(root)
    icon: SmartConnect.stateIcon()
    autoHide: false
    text: SmartConnect.connected && SmartConnect.battery >= 0 ? SmartConnect.battery + "%" : ""
    tooltipText: !SmartConnect.daemonAvailable ? "Smart Connect: daemon not running"
               : SmartConnect.connected ? SmartConnect.phoneName + " connected"
               : SmartConnect.paired ? SmartConnect.phoneName + " disconnected" : "Smart Connect: no phone paired"
    onClicked: if (pluginApi) pluginApi.openPanel(root.screen, this)
  }
}
