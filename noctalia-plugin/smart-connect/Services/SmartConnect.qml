pragma Singleton

import QtQuick 6.11
import Quickshell
import Quickshell.Io
import qs.Commons

// Talks to smartconnectd over its control socket ($XDG_RUNTIME_DIR/smartconnectd.sock):
// a long-lived `subscribe` stream for status, plus a command socket for connect/disconnect/pair.
QtObject {
  id: root

  readonly property string socketPath: Quickshell.env("XDG_RUNTIME_DIR")
    ? (Quickshell.env("XDG_RUNTIME_DIR") + "/smartconnectd.sock")
    : (Quickshell.env("HOME") + "/.local/state/smartconnect-linux/run/smartconnectd.sock")

  property bool daemonAvailable: false
  property var status: ({})
  readonly property bool connected: daemonAvailable && (status.connected ?? false)
  readonly property bool paired: status.paired ?? false
  readonly property string phoneName: status.name || status.model || "Phone"
  readonly property int battery: status.battery ?? -1
  readonly property bool charging: status.charging ?? false
  readonly property string wallpaper: status.wallpaper ? "file://" + status.wallpaper : ""
  readonly property string pairingQr: status.pairingQr ? "file://" + status.pairingQr : ""

  readonly property var notifications: status.notifications || []
  // Key of the notification whose reply box should be focused (set from the desktop "Reply" button).
  property string replyKey: ""

  property bool busy: false
  property string lastMessage: ""
  property bool showPairing: false

  function connectPhone(): void { send("connect"); }
  function disconnectPhone(): void { send("disconnect"); }
  function pairNewPhone(): void { showPairing = true; send("pair"); }

  function reply(key: string, text: string): void {
    if (!commandAvailable || text.trim() === "") return;
    busy = true;
    commandSocket.write(JSON.stringify({ cmd: "reply", key: key, text: text }) + "\n");
    if (replyKey === key) replyKey = "";
  }

  function send(cmd: string): void {
    if (!commandAvailable) {
      lastMessage = "smartconnectd is not running";
      return;
    }
    busy = true;
    commandSocket.write(cmd + "\n");
  }

  // ── Screen mirroring: runs the smartconnect-mirror viewer as a child process ──
  // Helpers are run by command name, so they are looked up on PATH (packaging/install.sh puts them
  // in ~/.local/bin), unless the plugin settings give a path (the viewer and the display helper).
  readonly property string defaultMirrorBinary: "smartconnect-mirror"
  readonly property bool mirroring: mirrorProcess.running
  property string mirrorStatus: ""
  property bool mirrorStarted: false
  property bool mirrorStopRequested: false
  property string mirrorLastStderr: ""

  function expandHome(path: string): string {
    const p = (path || "").trim() || defaultMirrorBinary;
    if (p === "~") return Quickshell.env("HOME");
    if (p.startsWith("~/")) return Quickshell.env("HOME") + p.substring(1);
    return p;
  }

  function startMirror(binary: string): void {
    if (mirrorProcess.running) return;
    if (!connected) {
      mirrorStatus = "Connect the phone first";
      return;
    }
    mirrorStatus = "";
    mirrorLastStderr = "";
    mirrorStarted = false;
    mirrorStopRequested = false;
    mirrorProcess.command = [expandHome(binary)];
    mirrorProcess.running = true;
  }

  function stopMirror(): void {
    if (!mirrorProcess.running) return;
    mirrorStopRequested = true;
    mirrorProcess.signal(2); // SIGINT: the viewer stops the phone stream and exits
  }

  function toggleMirror(binary: string): void {
    if (mirrorProcess.running) stopMirror();
    else startMirror(binary);
  }

  property Process mirrorProcess: Process {
    onStarted: root.mirrorStarted = true
    onExited: (exitCode, exitStatus) => {
      if (root.mirrorStopRequested || exitCode === 0) root.mirrorStatus = "";
      else root.mirrorStatus = "Mirroring stopped (exit " + exitCode + ")" + (root.mirrorLastStderr ? ": " + root.mirrorLastStderr : "");
    }
    onRunningChanged: {
      if (!running && !root.mirrorStarted && !root.mirrorStopRequested && root.mirrorStatus === "")
        root.mirrorStatus = "Could not start viewer: " + (command[0] || "") + " (not installed or not on PATH? See the plugin settings)";
    }
    stderr: SplitParser {
      onRead: data => { if (data.trim() !== "") root.mirrorLastStderr = data.trim().substring(0, 200); }
    }
  }

  // ── Cross Control: runs smartconnect-crosscontrol (pointer/keyboard to the phone) ──
  readonly property string crossBinary: "smartconnect-crosscontrol"
  readonly property var crossEdges: ["right", "left", "top", "bottom"]
  readonly property bool crossControlling: crossProcess.running
  property string crossStatus: ""
  property bool crossStarted: false
  property bool crossStopRequested: false
  property string crossLastStderr: ""

  function startCross(edge: string): void {
    if (crossProcess.running) return;
    if (!connected) {
      crossStatus = "Connect the phone first";
      return;
    }
    crossStatus = "";
    crossLastStderr = "";
    crossStarted = false;
    crossStopRequested = false;
    crossProcess.command = [expandHome(crossBinary), "--edge", crossEdges.indexOf(edge) >= 0 ? edge : "right"];
    crossProcess.running = true;
  }

  function stopCross(): void {
    if (!crossProcess.running) return;
    crossStopRequested = true;
    crossProcess.signal(2); // SIGINT: tells the phone to stop, then exits
  }

  function toggleCross(edge: string): void {
    if (crossProcess.running) stopCross();
    else startCross(edge);
  }

  property Process crossProcess: Process {
    onStarted: root.crossStarted = true
    onExited: (exitCode, exitStatus) => {
      if (root.crossStopRequested || exitCode === 0) root.crossStatus = "";
      else root.crossStatus = "Cross Control stopped (exit " + exitCode + ")" + (root.crossLastStderr ? ": " + root.crossLastStderr : "");
    }
    onRunningChanged: {
      if (!running && !root.crossStarted && !root.crossStopRequested && root.crossStatus === "")
        root.crossStatus = "Could not start Cross Control: " + (command[0] || "") + " (not installed or not on PATH?)";
    }
    stderr: SplitParser {
      onRead: data => { if (data.trim() !== "") root.crossLastStderr = data.trim().substring(0, 200); }
    }
  }

  // ── Phone as webcam: runs smartconnect-webcam (svc 3) ──
  readonly property string defaultWebcamBinary: "smartconnect-webcam"
  property bool webcamRunning: false
  property string webcamStatus: ""
  property bool webcamStopRequested: false
  property string webcamLastStderr: ""

  function startWebcam(binary: string): void {
    if (webcamRunning) return;
    if (!connected) {
      webcamStatus = "Connect the phone first";
      return;
    }
    webcamStatus = "";
    webcamLastStderr = "";
    webcamStopRequested = false;
    const bin = expandHome((binary || "").trim() || defaultWebcamBinary);
    Quickshell.execDetached([bin, "--start"]);
    probeWebcam();
  }

  function stopWebcam(binary: string): void {
    webcamStopRequested = true;
    const bin = expandHome((binary || "").trim() || defaultWebcamBinary);
    Quickshell.execDetached([bin, "--stop"]);
    probeWebcam();
  }

  function toggleWebcam(binary: string): void {
    if (webcamRunning) stopWebcam(binary);
    else startWebcam(binary);
  }

  function switchWebcam(binary: string): void {
    const bin = expandHome((binary || "").trim() || defaultWebcamBinary);
    Quickshell.execDetached([bin, "--switch-camera"]);
  }

  function toggleWebcamTorch(binary: string): void {
    const bin = expandHome((binary || "").trim() || defaultWebcamBinary);
    Quickshell.execDetached([bin, "--torch"]);
  }

  function probeWebcam(): void {
    webcamProbe.command = ["sh", "-c", 'if pgrep -x smartconnect-webcam >/dev/null 2>&1; then echo "running"; else echo "stopped"; fi'];
    webcamProbe.running = true;
  }

  property Process webcamProbe: Process {
    stdout: SplitParser {
      onRead: data => {
        root.webcamRunning = data.trim() === "running";
      }
    }
  }

  property Timer webcamTimer: Timer {
    interval: 2000
    repeat: true
    running: true
    onTriggered: root.probeWebcam()
  }

  // ── Phone as display: smartconnect-display shows this PC's screen on the phone ──
  // Choices: $XDG_CONFIG_HOME/smartconnect-linux/display.json, shared with the GTK app and read by
  // smartconnect-display itself (a start from the phone uses them).
  // Sessions start detached (a plugin reload or shell restart would SIGKILL a tracked Process, leaving
  // the virtual output on) through the same wrapper as the GTK app: the log's first line is "pid N",
  // so either front-end shows the live state of a session the other one started.
  readonly property string defaultDisplayBinary: "smartconnect-display"
  readonly property string configHome: Quickshell.env("XDG_CONFIG_HOME") || (Quickshell.env("HOME") + "/.config")
  readonly property string displayConfigPath: configHome + "/smartconnect-linux/display.json"
  readonly property string displayLogPath: (Quickshell.env("XDG_RUNTIME_DIR") || "/tmp") + "/smartconnect-display.log"
  readonly property var displayDefaults: ({ mode: "extend", size: "native", displayScale: 2.0, oskHeight: 45, output: null, audio: false, followRotation: false, bitrate: 8000, fps: 60 })
  property var displayConfig: displayDefaults
  // Running sessions (ours or not): [{ pid, mode }].
  property var displaySessions: []
  readonly property bool displayRunning: displaySessions.length > 0
  readonly property string displayRunningMode: displaySessions.length > 0 ? displaySessions[0].mode : ""
  // A start or stop was asked for and its effect is not visible yet.
  property string displayBusy: ""
  property double displayBusySince: 0
  // The session this plugin started, once its log names it (for the fatal-error stop below).
  property int displayOwnPid: 0
  property string displayError: ""
  property var displayLog: ({})
  // File name the running sessions are recognised by (from the configured binary).
  property string displayProgram: "smartconnect-display"
  // The panel is open: keep the state fresh.
  property bool displayWatch: false
  property string displayPendingAction: ""
  property string displayPendingBinary: ""
  property string displayRestartBinary: ""
  property bool displayRestart: false

  readonly property bool displayLogged: displayLog.pid !== undefined && displaySessions.some(s => s.pid === displayLog.pid)
  readonly property string displayStatus: {
    if (displayError !== "") return displayError;
    if (displayBusy === "stop" || displayRestart) return displayRestart ? "Restarting…" : "Stopping…";
    if (displayRunning && displayLogged) {
      const l = displayLog;
      if (l.error) return l.error;
      if (l.stopped) return "Stopping…";
      if (l.streaming) return "Streaming" + (l.size ? " " + l.size.replace("x", "×") : "") + (l.output ? " · " + l.output : "");
      if (l.lost) return "Connection interrupted, waiting for the phone…";
      return l.size ? "Connecting…" : "Waiting for the phone…";
    }
    if (displayBusy === "start") return "Starting…";
    if (displayRunning) return "Running (started outside Smart Connect)";
    return displayOutcome();
  }
  // A "connect the phone first" (or any earlier error) is moot once the phone is back.
  onConnectedChanged: if (connected) {
    displayError = "";
    if (filesStatus === "Connect the phone first") filesStatus = "";
  }

  readonly property bool displayStatusIsError: displayError !== "" || (displayLogged && !!displayLog.error)
                                               || displayStatus.startsWith("Last session failed")

  function displayOutcome(): string {
    const l = displayLog;
    if (l.pid === undefined) return "";
    // A fatal error says why; the stop that follows it ("could not start the stream") only says that.
    if (l.fatal && l.error) return "Last session failed: " + l.error;
    const reasons = {
      "the phone stopped screen sharing": "stopped on the phone",
      "the phone reports an ICE failure": "the connection to the phone failed (ICE)",
      "refused by the phone": "the phone refused the session",
      "daemon went away": "smartconnectd stopped"
    };
    if (l.stopped && l.stopped !== "signal") return "Last session: " + (reasons[l.stopped] || l.stopped);
    if (l.error) return "Last session failed: " + l.error;
    return "";
  }

  function parseDisplayConfig(text: string): var {
    let c = {};
    try { c = JSON.parse(text) || {}; } catch (e) { c = {}; }
    const d = displayDefaults;
    return {
      mode: (c.mode === "extend" || c.mode === "mirror") ? c.mode : d.mode,
      size: (typeof c.size === "string" && /^(native|requested|\d+x\d+)$/.test(c.size)) ? c.size : d.size,
      displayScale: (typeof c.displayScale === "number" && c.displayScale >= 0.5 && c.displayScale <= 4) ? c.displayScale : d.displayScale,
      oskHeight: (typeof c.oskHeight === "number" && c.oskHeight >= 15 && c.oskHeight <= 80) ? Math.round(c.oskHeight) : d.oskHeight,
      output: (typeof c.output === "string" && c.output !== "") ? c.output : null,
      audio: c.audio === true,
      followRotation: c.followRotation === true,
      // Set in the GTK app (Stream quality, Frame rate); kept here so a save doesn't drop them.
      bitrate: (typeof c.bitrate === "number" && c.bitrate >= 500 && c.bitrate <= 50000) ? Math.round(c.bitrate) : d.bitrate,
      fps: (typeof c.fps === "number" && c.fps >= 10 && c.fps <= 60) ? Math.round(c.fps) : d.fps
    };
  }

  // Merges `changes` into display.json (written atomically; the directory may not exist yet).
  function saveDisplayConfig(changes: var): void {
    const c = Object.assign({}, displayConfig, changes);
    displayConfig = c;
    const json = JSON.stringify({
      mode: c.mode, size: c.size, displayScale: c.displayScale, oskHeight: c.oskHeight,
      output: c.output, audio: c.audio, followRotation: c.followRotation,
      bitrate: c.bitrate, fps: c.fps
    }, null, 2);
    Quickshell.execDetached(["sh", "-c", 'mkdir -p "$(dirname "$1")" && printf "%s\\n" "$2" > "$1.tmp" && mv "$1.tmp" "$1"',
                             "sh", displayConfigPath, json]);
  }

  // Flags for display.json's choices. --osk-height only when not the default (an older helper does not
  // know it); audio and rotation reach the helper through display.json only.
  function displayArgs(): var {
    const c = displayConfig;
    let a = ["--mode", c.mode];
    if (c.size !== "requested") a.push("--size", c.size);
    if (c.mode === "extend") {
      a.push("--display-scale", String(c.displayScale));
      if (c.output) a.push("--output", c.output);
    }
    if (c.oskHeight !== 45) a.push("--osk-height", String(c.oskHeight));
    return a;
  }

  function startDisplay(binary: string): void {
    if (displayRunning || displayBusy !== "") return;
    if (!connected) {
      displayError = "Connect the phone first";
      return;
    }
    displayError = "";
    const bin = expandHome((binary || "").trim() || defaultDisplayBinary);
    displayProgram = bin.split("/").pop();
    Quickshell.execDetached(["sh", "-c", 'exec >"$1" 2>&1; echo "pid $$"; shift; exec "$@"', "sh", displayLogPath, bin].concat(displayArgs()));
    displayBusy = "start";
    displayBusySince = Date.now();
    checkDisplay();
  }

  // SIGINT: the helper tells the phone to stop and switches an extend output off.
  function stopDisplay(): void {
    if (displaySessions.length === 0) return;
    displayError = "";
    Quickshell.execDetached(["kill", "-INT"].concat(displaySessions.map(s => String(s.pid))));
    displayBusy = "stop";
    displayBusySince = Date.now();
    checkDisplay();
  }

  // `toggle`, `start`, `stop`, or `extend`/`duplicate` (save that mode and start, restarting a session
  // in the other mode). Runs once the process list is fresh, so a session started elsewhere counts.
  function displayCommand(action: string, binary: string): void {
    displayPendingAction = action;
    displayPendingBinary = binary || "";
    if ((binary || "").trim() !== "") displayProgram = expandHome(binary.trim()).split("/").pop();
    checkDisplay();
  }

  function runDisplayAction(action: string, binary: string): void {
    if (action === "stop") return stopDisplay();
    if (action === "start") return startDisplay(binary);
    if (action === "extend" || action === "duplicate") {
      const mode = action === "extend" ? "extend" : "mirror";
      if (displayConfig.mode !== mode) saveDisplayConfig({ mode: mode });
      if (!displayRunning) return startDisplay(binary);
      if (displayRunningMode !== mode) {
        displayRestart = true;
        displayRestartBinary = binary;
        stopDisplay();
      }
      return;
    }
    if (displayRunning) stopDisplay();
    else startDisplay(binary);
  }

  // display.json follows file-change notifications; this also catches a file created while unwatched
  // (called when the panel or the settings open).
  function refreshDisplayConfig(): void {
    displayConfigFile.reload();
  }

  // Like the GTK app: the kernel's short process name (15 bytes) first, then argv[0], or argv[1] for
  // a helper run by an interpreter.
  function checkDisplay(): void {
    displayLogFile.reload();
    if (displayCheck.running) return;
    displayCheck.command = ["pgrep", "-a", "-x", displayProgram.substring(0, 15)];
    displayCheck.running = true;
  }

  function applyDisplaySessions(text: string): void {
    const sessions = [];
    for (const line of text.split("\n")) {
      const m = /^(\d+) (.*)$/.exec(line.trim());
      if (!m) continue;
      const argv = m[2].split(" ");
      if (!argv.slice(0, 2).some(a => a.split("/").pop() === displayProgram)) continue;
      // Without --mode (e.g. started from the phone) the helper takes it from display.json.
      const flag = / --mode (extend|mirror)( |$)/.exec(" " + m[2]);
      sessions.push({ pid: parseInt(m[1], 10), mode: flag ? flag[1] : displayConfig.mode });
    }
    displaySessions = sessions;
    const age = Date.now() - displayBusySince;
    // Starting ends once the log names a running session (or it gave up): until then the log may still
    // be the previous session's, and the state would read "started outside Smart Connect".
    if (displayBusy === "start" && (displayLogged || age > 5000)) {
      if (displayLogged) displayOwnPid = displayLog.pid;
      displayBusy = "";
    }
    if (displayOwnPid > 0 && !sessions.some(s => s.pid === displayOwnPid)) displayOwnPid = 0;
    if (displayBusy === "stop" && (sessions.length === 0 || age > 10000)) {
      if (sessions.length > 0) displayError = "The phone display did not stop; try again";
      displayBusy = "";
      if (sessions.length === 0 && displayRestart) displayRestartTimer.restart();
      else displayRestart = false;
    }
    // A session started here hit an error it cannot recover from: stop it, so the phone does not
    // wait on "Connecting…" forever.
    if (displayOwnPid > 0 && displayLog.pid === displayOwnPid && displayLog.fatal && !displayLog.stopped && displayBusy === "") {
      displayOwnPid = 0;
      stopDisplay();
    }
    if (displayPendingAction !== "") {
      const action = displayPendingAction;
      displayPendingAction = "";
      runDisplayAction(action, displayPendingBinary);
    }
  }

  function parseDisplayLog(text: string): var {
    const lines = text.split("\n");
    const head = /^pid (\d+)$/.exec((lines[0] || "").trim());
    if (!head) return {};
    const s = { pid: parseInt(head[1], 10), size: "", output: "", streaming: false, lost: false, stopped: "", error: "", fatal: false };
    for (let i = 1; i < lines.length; i++) {
      const l = lines[i].trim();
      let r;
      if ((r = /^stopping: (.*)$/.exec(l))) s.stopped = r[1];
      else if (l.startsWith("phone panel ") && (r = /sending (\d+x\d+)$/.exec(l))) s.size = r[1];
      else if ((r = /^stream size → (\d+x\d+)/.exec(l))) s.size = r[1];
      else if ((r = /^capturing (\S+) \(/.exec(l))) s.output = r[1];
      else if ((r = /^peer connection: (\w+)/.exec(l))) {
        if (r[1] === "Connected") { s.streaming = true; s.lost = false; s.error = ""; }
        else if (r[1] === "Failed" || r[1] === "Disconnected" || r[1] === "Closed") { s.lost = s.lost || s.streaming; s.streaming = false; }
      }
      else if ((r = /^svc 202: (.*)$/.exec(l))) { s.error = r[1]; s.fatal = true; }
      else if ((r = /^Error: (.*)$/.exec(l))) s.error = r[1];
      else if (l.startsWith("phone refused: ")) s.error = "the phone refused the session";
      else if (l.startsWith("sh:") && l.indexOf("exec") >= 0) s.error = l;
    }
    return s;
  }

  property Process displayCheck: Process {
    stdout: StdioCollector {
      onStreamFinished: root.applyDisplaySessions(text)
    }
  }

  property FileView displayConfigFile: FileView {
    path: root.displayConfigPath
    watchChanges: true
    printErrors: false
    onFileChanged: reload()
    onLoaded: root.displayConfig = root.parseDisplayConfig(text())
    // Missing (not saved yet) or unreadable: keep what is in memory. Resetting here would undo a
    // save that raced this load.
  }

  property FileView displayLogFile: FileView {
    path: root.displayLogPath
    printErrors: false
    onLoaded: root.displayLog = root.parseDisplayLog(text())
    onLoadFailed: root.displayLog = ({})
  }

  property Timer displayTimer: Timer {
    interval: root.displayBusy !== "" || root.displayRestart ? 500 : (root.displayWatch ? 2000 : 4000)
    repeat: true
    running: root.displayBusy !== "" || root.displayRestart || root.displayRunning || root.displayWatch
    triggeredOnStart: true
    onTriggered: root.checkDisplay()
  }

  // After a stop for a restart: give the phone a moment to close its display view.
  property Timer displayRestartTimer: Timer {
    interval: 1500
    onTriggered: {
      root.displayRestart = false;
      root.startDisplay(root.displayRestartBinary);
    }
  }

  // ── Phone files: smartconnect-files mounts the phone's storage as a read-only FUSE folder ──
  // The `mount` subcommand blocks until the mount is released, so it runs as a tracked Process. The
  // helper survives phone reconnects and daemon restarts on its own (it re-handshakes), so the mount
  // stays up while the phone is briefly away. Whether the folder is really mounted is decided by
  // probing the folder itself (`filesProbeScript`), not by the process alone: `mountpoint -q` alone
  // calls a *stale* FUSE mount (its helper died: "Transport endpoint is not connected") mounted, and
  // `xdg-open` on it silently fails. A stale mount is cleared with `fusermount3 -uz`; a live mount
  // this plugin didn't start (an earlier shell, the GTK app) is adopted so it can be opened and
  // unmounted. Default folder ~/Phone, matching the GTK app; the panel passes its settings in.
  readonly property string defaultFilesBinary: "smartconnect-files"
  readonly property string defaultFilesDir: "~/Phone"
  property string filesMountpoint: ""
  property bool filesMounted: false
  readonly property bool filesMounting: filesProcess.running && !filesMounted
  property string filesStatus: ""
  property bool filesStatusError: false
  property bool filesStopRequested: false
  property string filesLastStderr: ""
  property double filesStartedAt: 0
  // Last probe result: live | offline | stale | none | unresponsive ("" = not probed).
  property string filesHealth: ""
  // A live mount at the folder that this plugin's process did not start (adopted).
  property bool filesExternal: false
  // Probe once to adopt or clear whatever is at the folder (the panel just opened).
  property bool filesAdopting: false
  // Open was asked for: open once a deep probe says the mount is live (remounting first if needed).
  property bool filesOpenAfter: false
  property bool filesProbeDeep: false
  property string filesProbeOut: ""
  property string filesBinaryLast: ""
  property string filesDirLast: ""

  // Prints one word for "$1": stale (helper gone), offline (our helper answers EIO: phone away, it
  // reconnects by itself), none (not a mount), unresponsive (timed out), or live. "$2" = deep also
  // lists the folder (that reaches the phone); shallow is `stat -f` + `mountpoint` only. `stat -f`
  // (statfs) is the stale test: plain `stat` keeps answering from the kernel's attribute cache for up
  // to the attribute TTL after the helper died, statfs fails at once ("Transport endpoint is not
  // connected"), and our helper answers it without asking the phone. LC_ALL=C keeps texts matchable.
  readonly property string filesProbeScript: '
    export LC_ALL=C
    d=$1
    err=$(timeout 2 stat -f -- "$d" 2>&1 >/dev/null); rc=$?
    [ $rc -eq 124 ] && { echo unresponsive; exit 0; }
    case "$err" in *"not connected"*) echo stale; exit 0;; esac
    err=$(timeout 2 stat -- "$d" 2>&1 >/dev/null); rc=$?
    [ $rc -eq 124 ] && { echo unresponsive; exit 0; }
    if [ $rc -ne 0 ]; then
      case "$err" in *"not connected"*) echo stale;; *"Input/output error"*) echo offline;; *) echo none;; esac
      exit 0
    fi
    mountpoint -q -- "$d" || { echo none; exit 0; }
    if [ "$2" = deep ]; then
      out=$(timeout 3 ls -- "$d" 2>&1 >/dev/null); rc=$?
      [ $rc -eq 124 ] && { echo unresponsive; exit 0; }
      case "$out" in *"not connected"*) echo stale; exit 0;; esac
      [ $rc -ne 0 ] && { echo offline; exit 0; }
    fi
    echo live'
  // Lazily unmounts "$1" only if it is a stale FUSE mount (never someone else's live mount).
  readonly property string filesClearStaleScript:
    'LC_ALL=C timeout 2 stat -f -- "$1" 2>&1 >/dev/null | grep -q "not connected" && fusermount3 -uz "$1"; true'

  function filesResolve(dir: string): string {
    return expandHome((dir || "").trim() || defaultFilesDir);
  }

  function setFilesStatus(text: string, error: bool): void {
    filesStatus = text;
    filesStatusError = error;
  }

  // The panel opened: remember its settings and look at the folder (adopt a live mount, clear a
  // stale one) unless our own mount is already tracked.
  function syncFiles(binary: string, dir: string): void {
    filesBinaryLast = binary;
    filesDirLast = dir;
    if (filesProcess.running || filesMounted) return;
    filesMountpoint = filesResolve(dir);
    filesAdopting = true;
    probeFiles(false);
  }

  function probeFiles(deep: bool): void {
    if (filesProbe.running || filesMountpoint === "") return;
    filesProbeDeep = deep;
    filesProbeOut = "";
    filesProbe.command = ["sh", "-c", filesProbeScript, "sh", filesMountpoint, deep ? "deep" : "shallow"];
    filesProbe.running = true;
  }

  function onFilesProbe(health: string): void {
    filesHealth = health;
    const dir = filesMountpoint;
    const up = health === "live" || health === "offline" || health === "unresponsive";
    if (health === "stale") {
      Quickshell.execDetached(["fusermount3", "-uz", dir]);
      if (filesProcess.running) { // a wedged helper: stop it too
        filesStopRequested = true;
        filesProcess.running = false;
      }
      filesMounted = false;
      filesExternal = false;
      setFilesStatus("Phone files: cleared a stale mount at " + dir + " (its helper had stopped)", false);
    } else if (!up) {
      if (filesExternal) { // unmounted by its owner meanwhile
        filesExternal = false;
        filesMounted = false;
      }
    } else if (!filesMounted && (filesProcess.running || filesAdopting)) {
      filesMounted = true; // Mounting → Mounted, or adopting a mount we found
      filesExternal = !filesProcess.running;
    }
    filesAdopting = false;

    if (!filesOpenAfter) return;
    if (!filesProbeDeep) {
      if (filesMounted) Qt.callLater(() => probeFiles(true)); // just mounted: check properly, then open
      return;
    }
    if (health === "live") {
      filesOpenAfter = false;
      filesOpener.command = ["xdg-open", dir];
      filesOpener.running = true;
    } else if (health === "offline") {
      filesOpenAfter = false;
      setFilesStatus("Phone files: the phone is offline; the mount reconnects when it is back", false);
    } else if (health === "unresponsive") {
      filesOpenAfter = false;
      setFilesStatus("Phone files: the phone's storage isn't responding, try again in a moment", true);
    } else if (filesProcess.running) {
      // Still mounting: the promotion above triggers another deep probe.
    } else if (connected) {
      startFiles(filesBinaryLast, filesDirLast, true); // remount, then open
    } else {
      filesOpenAfter = false;
      setFilesStatus("Connect the phone first", false);
    }
  }

  function startFiles(binary: string, dir: string, keepOpenRequest: bool): void {
    if (filesProcess.running) return;
    if (!connected) {
      setFilesStatus("Connect the phone first", false);
      return;
    }
    filesBinaryLast = binary;
    filesDirLast = dir;
    setFilesStatus("", false);
    filesLastStderr = "";
    filesMounted = false;
    filesExternal = false;
    filesHealth = "";
    filesStopRequested = false;
    if (!keepOpenRequest) filesOpenAfter = false;
    filesMountpoint = filesResolve(dir);
    filesStartedAt = Date.now();
    const bin = expandHome((binary || "").trim() || defaultFilesBinary);
    // Clear a stale mount first (an older helper can't mount over one), make the folder, then exec
    // the blocking mount.
    filesProcess.command = ["sh", "-c", filesClearStaleScript + '; mkdir -p "$1" 2>/dev/null; exec "$2" mount --mountpoint "$1"',
                            "sh", filesMountpoint, bin];
    filesProcess.running = true;
  }

  // fusermount3 -u releases the FUSE mount; the blocking `mount` process then exits on its own
  // (the helper also unmounts cleanly on SIGTERM). An adopted mount is just released.
  function stopFiles(): void {
    filesOpenAfter = false;
    if (filesProcess.running) {
      filesStopRequested = true;
      Quickshell.execDetached(["sh", "-c", 'fusermount3 -u "$1" || fusermount3 -uz "$1"', "sh", filesMountpoint]);
    } else if (filesExternal) {
      Quickshell.execDetached(["sh", "-c", 'fusermount3 -u "$1" || fusermount3 -uz "$1"', "sh", filesMountpoint]);
      filesExternal = false;
      filesMounted = false;
    }
  }

  function toggleFiles(binary: string, dir: string): void {
    if (filesProcess.running || filesExternal) stopFiles();
    else startFiles(binary, dir, false);
  }

  // Open: check the mount first (a deep probe); `onFilesProbe` opens it, says why not, or
  // remounts first.
  function openFiles(binary: string, dir: string): void {
    filesBinaryLast = binary;
    filesDirLast = dir;
    if (filesMountpoint === "") filesMountpoint = filesResolve(dir);
    filesOpenAfter = true;
    if (filesMounting) return; // opens once mounted
    probeFiles(true);
  }

  property Process filesProcess: Process {
    onExited: (exitCode, exitStatus) => {
      root.filesMounted = false;
      root.filesHealth = "";
      // However it ended, never leave a stale mount behind.
      Quickshell.execDetached(["sh", "-c", root.filesClearStaleScript, "sh", root.filesMountpoint]);
      if (root.filesStopRequested || exitCode === 0) {
        if (!root.filesStatus.startsWith("Phone files: cleared")) root.setFilesStatus("", false);
      } else {
        root.setFilesStatus("Phone files: mount failed (exit " + exitCode + ")" + (root.filesLastStderr ? ": " + root.filesLastStderr : ""), true);
        root.filesOpenAfter = false;
      }
      root.filesStopRequested = false;
    }
    stderr: SplitParser {
      onRead: data => { if (data.trim() !== "") root.filesLastStderr = data.trim().substring(0, 200); }
    }
  }

  property Process filesProbe: Process {
    stdout: SplitParser {
      onRead: data => { if (data.trim() !== "") root.filesProbeOut = data.trim(); }
    }
    onExited: (exitCode, exitStatus) => root.onFilesProbe(root.filesProbeOut || "none")
  }

  // xdg-open, so a failure is reported instead of doing nothing.
  property Process filesOpener: Process {
    onExited: (exitCode, exitStatus) => {
      if (exitCode !== 0) root.setFilesStatus("Phone files: could not open " + root.filesMountpoint + " (xdg-open exit " + exitCode + ")", true);
    }
  }

  // While mounting: promote once the folder is a live mount, with a grace fallback where the probe
  // tools are missing. While mounted: a cheap probe every 5 s catches a mount gone stale.
  property Timer filesTimer: Timer {
    interval: 500
    repeat: true
    running: root.filesProcess.running && !root.filesMounted
    onTriggered: {
      root.probeFiles(false);
      if (Date.now() - root.filesStartedAt > 6000 && root.filesProcess.running && !root.filesMounted) {
        root.filesMounted = true;
        if (root.filesOpenAfter) root.probeFiles(true);
      }
    }
  }
  property Timer filesHealthTimer: Timer {
    interval: 5000
    repeat: true
    running: root.filesMounted
    onTriggered: root.probeFiles(false)
  }

  // ── Per-app notification mute (desktop popups only; the daemon keeps the list) ──
  property var mutedApps: []

  function loadMuted(): void {
    if (commandAvailable) commandSocket.write(JSON.stringify({ cmd: "notif_mute" }) + "\n");
  }

  function setAppMuted(pkg: string, mute: bool): void {
    if (!commandAvailable || !pkg) return;
    commandSocket.write(JSON.stringify({ cmd: "notif_mute", pkg: pkg, mute: mute }) + "\n");
  }

  function isMuted(pkg: string): bool {
    return !!pkg && mutedApps.indexOf(pkg) >= 0;
  }

  // App name for a package, from the notifications seen.
  function appName(pkg: string): string {
    const n = notifications.find(n => n.pkg === pkg && n.app);
    return n ? n.app : pkg;
  }

  function batteryIcon(): string {
    if (battery < 0) return "battery-exclamation";
    if (charging) return "battery-charging-2";
    if (battery < 5) return "battery";
    if (battery < 25) return "battery-1";
    if (battery < 50) return "battery-2";
    if (battery < 75) return "battery-3";
    return "battery-4";
  }

  function stateIcon(): string {
    if (!daemonAvailable) return "device-mobile-off";
    return connected ? "device-mobile" : "device-mobile-x";
  }

  property bool commandAvailable: false

  // Quickshell's Socket never reconnects once its peer has gone: toggling `connected` or resetting
  // `path` on the same object doesn't redial after the daemon restarts and replaces its socket
  // file (verified in an isolated Quickshell). So every attempt gets a fresh Socket. It is created
  // disconnected, stored, and only then connected: a local connect can complete synchronously, and
  // signals from a socket that is no longer the current one are ignored.
  property var statusSocket: null
  property var commandSocket: null

  property Component statusSocketComponent: Component {
    Socket {
      id: sock
      path: root.socketPath
      connected: false
      onConnectionStateChanged: {
        if (sock !== root.statusSocket) return;
        root.daemonAvailable = connected;
        if (connected) write("subscribe\n");
        else root.status = ({});
      }
      onError: if (sock === root.statusSocket) root.daemonAvailable = false
      parser: SplitParser {
        onRead: data => {
          try { root.status = JSON.parse(data); } catch (e) { Logger.w("SmartConnect", "bad status line: " + data); }
        }
      }
    }
  }

  property Component commandSocketComponent: Component {
    Socket {
      id: sock
      path: root.socketPath
      connected: false
      onConnectionStateChanged: {
        if (sock !== root.commandSocket) return;
        root.commandAvailable = connected;
        if (connected) root.loadMuted();
      }
      onError: if (sock === root.commandSocket) root.commandAvailable = false
      parser: SplitParser {
        onRead: data => {
          let r = null;
          try { r = JSON.parse(data); } catch (e) { r = null; }
          // notif_mute replies carry the whole list and are not user commands.
          if (r && Array.isArray(r.muted)) {
            root.mutedApps = r.muted;
            return;
          }
          root.busy = false;
          root.lastMessage = r ? (r.message || "") : data;
        }
      }
    }
  }

  function redialStatus(): void {
    const old = statusSocket;
    statusSocket = statusSocketComponent.createObject(root);
    if (old) old.destroy();
    statusSocket.connected = true;
  }

  function redialCommand(): void {
    const old = commandSocket;
    commandSocket = commandSocketComponent.createObject(root);
    if (old) old.destroy();
    commandSocket.connected = true;
  }

  Component.onCompleted: {
    redialStatus();
    redialCommand();
  }

  // Reconnect while the daemon is down or was restarted (a restart replaces the socket file).
  property Timer reconnectTimer: Timer {
    interval: 3000
    repeat: true
    running: true
    onTriggered: {
      if (!root.daemonAvailable) root.redialStatus();
      if (!root.commandAvailable) root.redialCommand();
    }
  }
}
