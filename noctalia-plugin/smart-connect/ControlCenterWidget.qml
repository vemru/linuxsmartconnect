import QtQuick 6.11
import Quickshell
import qs.Widgets
import "./Services"

NIconButtonHot {
  property ShellScreen screen
  property var pluginApi: null

  icon: SmartConnect.stateIcon()
  tooltipText: SmartConnect.connected
    ? SmartConnect.phoneName + (SmartConnect.battery >= 0 ? " · " + SmartConnect.battery + "%" : "")
    : "Smart Connect: disconnected"
  onClicked: pluginApi?.togglePanel(screen, this)
}
