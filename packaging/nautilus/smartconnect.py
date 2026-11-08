# Nautilus python extension for Smart Connect
# Right-click files/folders -> "Send to phone"

import subprocess
from urllib.parse import unquote
from gi.repository import Nautilus, GObject

class SmartConnectExtension(GObject.GObject, Nautilus.MenuProvider):
    def __init__(self):
        super().__init__()

    def get_file_items(self, files):
        if not files:
            return []

        item = Nautilus.MenuItem(
            name="SmartConnect::SendToPhone",
            label="Send to phone",
            tip="Send selected file(s) to connected phone via Smart Connect",
            icon="phone-symbolic"
        )
        item.connect("activate", self._send_files, files)
        return [item]

    def _send_files(self, _menu, files):
        paths = []
        for file in files:
            uri = file.get_uri()
            if uri.startswith("file://"):
                paths.append(unquote(uri[7:]))
            elif file.is_gone():
                continue
            else:
                paths.append(file.get_location().get_path())

        if paths:
            cmd = ["smartconnect-send"] + paths
            subprocess.Popen(cmd)
