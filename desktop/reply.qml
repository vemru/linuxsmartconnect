// Quick-reply window for a phone notification. smartconnectd runs it with `qs -p` and passes
// the notification through env vars; Enter sends the reply over the daemon's control socket.
import QtQuick
import QtQuick.Controls
import QtQuick.Layouts
import Quickshell
import Quickshell.Io

FloatingWindow {
  id: win
  title: "Reply · " + Quickshell.env("SC_APP")
  implicitWidth: 460
  implicitHeight: 220
  color: "#1e1e24"
  // Closing the window from the compositor (close-window, title-bar X) only hides it; without this
  // the qs process would linger for the rest of the session.
  onClosed: Qt.quit()

  property string status: ""

  function send() {
    if (field.text.trim() === "") return;
    status = "Sending…";
    sock.write(JSON.stringify({ cmd: "reply", key: Quickshell.env("SC_KEY"), text: field.text }) + "\n");
  }

  Socket {
    id: sock
    path: Quickshell.env("SC_SOCKET")
    connected: true
    parser: SplitParser {
      onRead: data => {
        let r = {};
        try { r = JSON.parse(data); } catch (e) {}
        if (r.ok) Qt.quit();
        else win.status = r.message || "Failed to send";
      }
    }
  }

  ColumnLayout {
    anchors.fill: parent
    anchors.margins: 16
    spacing: 8

    Label {
      text: Quickshell.env("SC_TITLE")
      color: "#f5f5f5"
      font.bold: true
      font.pixelSize: 16
      elide: Text.ElideRight
      Layout.fillWidth: true
    }
    Label {
      text: Quickshell.env("SC_BODY")
      color: "#b8b8c0"
      wrapMode: Text.Wrap
      maximumLineCount: 3
      elide: Text.ElideRight
      Layout.fillWidth: true
    }
    Item { Layout.fillHeight: true }
    RowLayout {
      Layout.fillWidth: true
      TextField {
        id: field
        Layout.fillWidth: true
        placeholderText: "Reply…"
        focus: true
        onAccepted: win.send()
        Keys.onEscapePressed: Qt.quit()
      }
      Button { text: "Send"; onClicked: win.send() }
    }
    Label {
      visible: win.status !== ""
      text: win.status
      color: "#ffb4a0"
    }
  }

  Component.onCompleted: field.forceActiveFocus()
}
