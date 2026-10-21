#!/usr/bin/env python3
"""Prototype phone-screen mirror viewer (spec: evidence-2026-10-20/mirroring-spec.md).

Talks to smartconnectd over its control socket: `watch` svc 1/15 frames from the phone and `send`
replies. GStreamer webrtcbin is the WebRTC answerer; the phone offers one H.264/H.265 video track.

    svc1 cmd1 (start, view=1)  ->  phone: svc15 cmd2 setup  ->  PC: svc15 cmd1 (supportWebRTC=1)
    phone: svc15 cmd3 (SDP offer) -> PC: svc15 cmd4 (answer) ; both: svc15 cmd20 ICE candidates
    PC: svc15 cmd9 heartbeat every 30 s ; PC: svc15 cmd7 stop
"""
import json
import os
import re
import signal
import socket
import sys
import time

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstWebRTC", "1.0")
gi.require_version("GstSdp", "1.0")
from gi.repository import GLib, Gst, GstSdp, GstWebRTC  # noqa: E402

SOCK = os.path.join(os.environ.get("XDG_RUNTIME_DIR", "/tmp"), "smartconnectd.sock")
LAN_PREFIX = os.environ.get("SC_LAN_PREFIX", "192.168.")


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


class Daemon:
    """Two control-socket connections: one streams phone frames, one carries commands."""

    def __init__(self, on_frame):
        self.watch = socket.socket(socket.AF_UNIX)
        self.watch.connect(SOCK)
        self.watch.sendall(b'{"cmd":"watch","services":[1,15]}\n')
        self.cmd = socket.socket(socket.AF_UNIX)
        self.cmd.connect(SOCK)
        self.on_frame = on_frame
        self.buf = b""
        GLib.io_add_watch(self.watch.fileno(), GLib.IO_IN, self._read)
        GLib.io_add_watch(self.cmd.fileno(), GLib.IO_IN, self._drain)

    def send(self, service, data):
        self.cmd.sendall((json.dumps({"cmd": "send", "service": service, "data": data}) + "\n").encode())

    def _drain(self, *_):
        reply = self.cmd.recv(65536)
        for line in reply.decode(errors="replace").splitlines():
            if '"ok":false' in line:
                log("daemon:", line)
        return True

    def _read(self, *_):
        chunk = self.watch.recv(1 << 20)
        if not chunk:
            log("daemon closed the watch stream")
            GLib.idle_add(loop.quit)
            return False
        self.buf += chunk
        while b"\n" in self.buf:
            line, self.buf = self.buf.split(b"\n", 1)
            try:
                msg = json.loads(line)
            except ValueError:
                continue
            if "service" in msg:
                self.on_frame(msg["service"], msg.get("data") or {})
        return True


def h264_only(sdp: str) -> str:
    """Keep only H.264 payload types in the video m-line (the phone lists H.265 first)."""
    lines = sdp.replace("\r\n", "\n").split("\n")
    h264 = {m.group(1) for l in lines if (m := re.match(r"a=rtpmap:(\d+) H264/90000", l))}
    if not h264:
        log("WARNING: offer has no H.264; leaving codecs as offered")
        return sdp
    out = []
    for l in lines:
        if l.startswith("m=video"):
            parts = l.split(" ")
            l = " ".join(parts[:3] + [pt for pt in parts[3:] if pt in h264])
        elif m := re.match(r"a=(?:rtpmap|fmtp|rtcp-fb):(\d+)", l):
            if m.group(1) not in h264:
                continue
        out.append(l)
    return "\r\n".join(out)


class Mirror:
    def __init__(self):
        self.daemon = Daemon(self.on_frame)
        self.session_id = None
        self.display_id = -1
        self.pending_local = []
        self.answered = False
        self.mid = "0"
        self.pipe = Gst.Pipeline.new("mirror")
        self.webrtc = Gst.ElementFactory.make("webrtcbin", "webrtc")
        self.webrtc.set_property("bundle-policy", GstWebRTC.WebRTCBundlePolicy.MAX_BUNDLE)
        self.webrtc.connect("on-ice-candidate", self.on_local_candidate)
        self.webrtc.connect("pad-added", self.on_pad)
        self.webrtc.connect("notify::ice-connection-state", self.on_state)
        self.pipe.add(self.webrtc)
        self.pipe.set_state(Gst.State.PLAYING)

    # --- phone -> PC -----------------------------------------------------------------------
    def on_frame(self, svc, data):
        cmd = data.get("cmd")
        body = data.get("data") or {}
        if svc == 1:
            log(f"svc1 cmd {cmd}", {k: v for k, v in data.items() if k not in ("ver",)})
            return
        if cmd == 2:
            log("setup from phone:", body.get("pkg"), "source", body.get("source"))
            self.send15(1, {
                "pkg": body.get("pkg", "mirror"), "userId": body.get("userId", 0),
                "width": 1920, "height": 1080, "phyWidth": 344, "phyHeight": 194,
                "displayWidth": 1920, "displayHeight": 1080, "displayDensity": 160, "displayScale": 1.0,
                "align": 0, "instanceId": int(time.time() * 1000), "port": 0,
                "source": body.get("source", 1), "supportedMediaFormat": 1, "supportWebRTC": 1,
                "supportCustomDisplay": False, "vrSupportUnfolded": False,
            })
        elif cmd == 3:
            self.session_id = body.get("sessionId", self.session_id)
            if body.get("displayId", -1) not in (-1, None):
                self.display_id = body["displayId"]
            log(f"params: session {self.session_id} {body.get('width')}x{body.get('height')} display {self.display_id}")
            if body.get("webRTCData") and not self.answered:
                offer = json.loads(body["webRTCData"])
                self.handle_offer(offer["sdp"])
        elif cmd == 20:
            c = json.loads(body.get("data") or "{}")
            if c.get("candidate"):
                self.webrtc.emit("add-ice-candidate", int(c.get("label", 0)), c["candidate"])
        elif cmd == 8:
            log("phone state:", body.get("state"), "reason", body.get("reason"), body.get("msg") or "")
        elif cmd == 10:
            pass  # heartbeat ack
        else:
            log(f"svc15 cmd {cmd}", body)

    def handle_offer(self, sdp_text):
        sdp_text = h264_only(sdp_text)
        if m := re.search(r"a=mid:(\S+)", sdp_text):
            self.mid = m.group(1)
        _, sdp = GstSdp.SDPMessage.new_from_text(sdp_text)
        offer = GstWebRTC.WebRTCSessionDescription.new(GstWebRTC.WebRTCSDPType.OFFER, sdp)
        # Offer/answer synchronously on the main loop: creating the answer inside a promise
        # change-func (streaming thread) returned an empty description ("t=0 0") via the bindings.
        p = Gst.Promise.new()
        self.webrtc.emit("set-remote-description", offer, p)
        p.wait()
        p = Gst.Promise.new()
        self.webrtc.emit("create-answer", None, p)
        p.wait()
        answer = p.get_reply().get_value("answer")
        text = answer.sdp.as_text()
        self.webrtc.emit("set-local-description", answer, Gst.Promise.new())
        log("sending answer (H.264)" if "H264" in text else "sending answer", f"{len(text)} bytes")
        self.send15(4, {"sessionId": self.session_id, "error": 0, "isWebRTC": 1, "resize": 0, "userId": 0,
                        "webRTCData": json.dumps({"type": "answer", "sdp": text})})
        self.answered = True
        for c in self.pending_local:
            self.send_candidate(*c)
        self.pending_local.clear()
        GLib.timeout_add_seconds(30, self.heartbeat)
        self.heartbeat()

    # --- PC -> phone -----------------------------------------------------------------------
    def send15(self, cmd, data=None):
        msg = {"ver": "1", "cmd": cmd}
        if data is not None:
            msg["data"] = data
        self.daemon.send(15, msg)

    def on_local_candidate(self, _, mline, candidate):
        if LAN_PREFIX not in candidate:
            return  # skip docker/VPN interfaces: the phone would try every one
        if self.answered:
            self.send_candidate(mline, candidate)
        else:
            self.pending_local.append((mline, candidate))

    def send_candidate(self, mline, candidate):
        self.send15(20, {"sessionId": self.session_id, "type": "candidate", "userId": 0, "pkg": "mirror",
                         "data": json.dumps({"type": "candidate", "label": mline, "id": self.mid,
                                             "candidate": candidate})})

    def heartbeat(self):
        self.send15(9)
        return True

    def start(self):
        log("asking the phone to start mirroring")
        self.daemon.send(1, {"ver": "1.0.0", "cmd": 1, "view": 1, "reply": 0, "drawerMode": False})

    def stop(self):
        if self.session_id:
            self.send15(7, {"sessionId": self.session_id, "error": 0})
        self.pipe.set_state(Gst.State.NULL)

    # --- media -----------------------------------------------------------------------------
    def on_state(self, *_):
        log("ICE:", self.webrtc.get_property("ice-connection-state").value_nick)

    def on_pad(self, _, pad):
        if pad.direction != Gst.PadDirection.SRC:
            return
        log("video stream arrived:", pad.get_current_caps().to_string() if pad.get_current_caps() else "")
        sink = Gst.parse_bin_from_description(
            "rtph264depay ! h264parse ! decodebin ! videoconvert ! autovideosink sync=false", True)
        self.pipe.add(sink)
        sink.sync_state_with_parent()
        pad.link(sink.get_static_pad("sink"))


if __name__ == "__main__":
    Gst.init(None)
    loop = GLib.MainLoop()
    m = Mirror()
    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGINT, lambda: (m.stop(), loop.quit()))
    m.start()
    loop.run()
