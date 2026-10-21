import QtQuick 6.11
import Quickshell.Io
import "./Services"

Item {
  property var pluginApi: null

  // `qs ipc call plugin:smart-connect toggle|connect|disconnect|mirror` (for keybinds), and
  // `qs ipc call plugin:smart-connect display toggle|start|stop|extend|duplicate`.
  IpcHandler {
    target: "plugin:smart-connect"
    function toggle() {
      if (pluginApi) pluginApi.withCurrentScreen(screen => pluginApi.openPanel(screen));
    }
    function connect() { SmartConnect.connectPhone(); }
    // Called by smartconnectd when "Reply" is clicked on a phone notification.
    function reply(key: string) {
      SmartConnect.showPairing = false;
      SmartConnect.replyKey = key;
      if (pluginApi) pluginApi.withCurrentScreen(screen => pluginApi.openPanel(screen));
    }
    function disconnect() { SmartConnect.disconnectPhone(); }
    // Start/stop the screen-mirroring viewer.
    function mirror() { SmartConnect.toggleMirror(pluginApi?.pluginSettings?.mirrorBinary ?? ""); }
    // Phone as display (this PC's screen on the phone). extend/duplicate save that mode and start,
    // restarting a running session that uses the other mode.
    function display(action: string) {
      const known = ["toggle", "start", "stop", "extend", "duplicate"];
      SmartConnect.displayCommand(known.indexOf(action) >= 0 ? action : "toggle", pluginApi?.pluginSettings?.displayBinary ?? "");
    }
  }
}
