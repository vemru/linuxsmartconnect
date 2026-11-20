// Generic per-window rules for GNOME Shell 51 (ESM extension).
// Matching is AND across fields, OR within a list. `titleFile` is an exact match against a
// language-neutral token read from a file in this extension's directory; no substring matching.
// The shipped rule pins the Smart Connect call window and places it in the top-right corner.
//
// Rules are applied from signals instead of polling: the window manager `map` signal,
// per-window `notify::title` / `notify::app-id` / `notify::wm-class` changes, and once on the
// first frame after mapping (mutter may reposition a window there). A quiet 5s safety scan
// picks up windows that were already mapped before the extension loaded.
//
// The extension owns call UI only while the daemon grants it presenter status.
//
// Debug logging is controlled by a `debug.txt` file in the extension directory containing `1`.
// Toggle it and disable/enable the extension; no shell restart needed. Errors are always logged.
// Absolute privacy rule: never log caller names, numbers, or private details.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Clutter from 'gi://Clutter';
import St from 'gi://St';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';

let DEBUG = false;

function debug(message) {
  if (DEBUG) log(`window-rules: ${message}`);
}

function logError(message) {
  log(`window-rules: ${message}`);
}

class DaemonClient {
  constructor(options = {}) {
    this._socketPath =
      options.socketPath ?? `${GLib.get_user_runtime_dir()}/smartconnectd.sock`;
    this._cancellable = new Gio.Cancellable();
    this._connection = null;
    this._outStream = null;
    this._inStream = null;
    this._dataInputStream = null;
    this._reconnectId = 0;
    this._reconnectDelayMs = 1000;
    this._isConnecting = false;
    this._closed = false;
    this._registeredId = 0;
    this._onStatus = options.onStatus ?? null;
    this._onDisconnected = options.onDisconnected ?? null;
  }

  start() {
    this._closed = false;
    this._connect();
  }

  _connect() {
    if (this._closed || this._isConnecting || this._connection) return;
    this._isConnecting = true;

    try {
      const client = new Gio.SocketClient();
      const addr = Gio.UnixSocketAddress.new(this._socketPath);
      client.connect_async(addr, this._cancellable, (source, res) => {
        this._isConnecting = false;
        if (this._closed) return;
        try {
          this._connection = source.connect_finish(res);
          this._outStream = this._connection.get_output_stream();
          this._inStream = this._connection.get_input_stream();
          this._dataInputStream = new Gio.DataInputStream({
            base_stream: this._inStream,
          });
          this._reconnectDelayMs = 1000;
          this._send({ cmd: 'call_ui', role: 'extension' });
          this._readNextLine();
        } catch (e) {
          if (!this._cancellable.is_cancelled()) {
            debug(`socket: connect failed (${e.message}), will retry`);
            this._onDisconnect();
          }
        }
      });
    } catch (e) {
      this._isConnecting = false;
      debug(`socket: connect setup error (${e.message})`);
      this._scheduleReconnect();
    }
  }

  _scheduleReconnect() {
    if (this._closed || this._reconnectId) return;
    this._reconnectId = GLib.timeout_add(
      GLib.PRIORITY_DEFAULT,
      this._reconnectDelayMs,
      () => {
        this._reconnectId = 0;
        this._connect();
        return GLib.SOURCE_REMOVE;
      }
    );
    this._reconnectDelayMs = Math.min(this._reconnectDelayMs * 2, 10000);
  }

  _readNextLine() {
    if (this._closed || !this._dataInputStream) return;
    try {
      this._dataInputStream.read_line_async(
        GLib.PRIORITY_DEFAULT,
        this._cancellable,
        (source, res) => {
          if (this._closed) return;
          try {
            const [line] = source.read_line_finish_utf8(res);
            if (line !== null) {
              try {
                const message = JSON.parse(line);
                if (!this._registeredId) {
                  if (message.ok && message.presenter === 'extension' && Number.isInteger(message.id)) {
                    this._registeredId = message.id;
                    // The daemon's `subscribe` is a plain-text command, not a JSON command.
                    this._sendRaw('subscribe\n');
                  } else {
                    this._onDisconnect();
                    return;
                  }
                } else if (this._onStatus) {
                  this._onStatus(message);
                }
              } catch (err) {
                if (!this._cancellable.is_cancelled())
                  logError('socket: invalid daemon response');
                this._onDisconnect();
                return;
              }
              this._readNextLine();
            } else {
              debug('socket: daemon disconnected (EOF)');
              this._onDisconnect();
            }
          } catch (e) {
            if (!this._cancellable.is_cancelled()) {
              debug(`socket: read error (${e.message})`);
              this._onDisconnect();
            }
          }
        }
      );
    } catch (e) {
      debug(`socket: read_line_async setup error (${e.message})`);
      this._onDisconnect();
    }
  }

  _onDisconnect() {
    if (this._onDisconnected) {
      try {
        this._onDisconnected();
      } catch (e) {
        logError('socket disconnect handler failed');
      }
    }
    this._cleanupConnection();
    this._registeredId = 0;
    this._scheduleReconnect();
  }

  _cleanupConnection() {
    if (this._connection) {
      try {
        this._connection.close(null);
      } catch (e) {}
      this._connection = null;
    }
    this._outStream = null;
    this._inStream = null;
    this._dataInputStream = null;
  }

  _send(obj) {
    this._write(new GLib.Bytes(new TextEncoder().encode(`${JSON.stringify(obj)}\n`)));
  }

  _sendRaw(text) {
    this._write(new GLib.Bytes(new TextEncoder().encode(text)));
  }

  _write(bytes) {
    if (this._closed || !this._outStream) return;
    try {
      this._outStream.write_bytes_async(
        bytes,
        GLib.PRIORITY_DEFAULT,
        this._cancellable,
        (source, res) => {
          if (this._closed) return;
          try {
            source.write_bytes_finish(res);
          } catch (e) {
            if (!this._cancellable.is_cancelled()) {
              debug(`socket: write error (${e.message})`);
              this._onDisconnect();
            }
          }
        }
      );
    } catch (e) {
      logError('socket: write setup failed');
      this._onDisconnect();
    }
  }

  close() {
    this._closed = true;
    if (this._reconnectId) {
      GLib.source_remove(this._reconnectId);
      this._reconnectId = 0;
    }
    if (this._cancellable) {
      this._cancellable.cancel();
    }
    this._registeredId = 0;
    this._cleanupConnection();
  }
}

export default class WindowRulesExtension extends Extension {
  enable() {
    DEBUG = false;
    try {
      const [ok, bytes] = this.dir.get_child('debug.txt').load_contents(null);
      DEBUG = ok && new TextDecoder().decode(bytes).trim() === '1';
    } catch (e) {
      // No flag file: normal quiet operation.
    }
    this._rules = [];
    this._touched = new Set();
    this._known = new Set();
    this._signalIds = [];
    this._pendingSources = new Set();
    this._actionCancellables = new Set();
    this._callCard = null;
    this._callPill = null;
    this._callViewSignature = null;
    this._callStageSignalId = 0;
    this._callTimerId = 0;
    this._call = null;
    this._callOwner = false;
    this._collapsedCallKey = null;
    this._elapsedBase = 0;
    this._elapsedStart = 0;
    this._loadRules();

    this._daemonClient = new DaemonClient({
      onStatus: (status) => this._onDaemonStatus(status),
      onDisconnected: () => this._clearCallUI(true),
    });
    this._daemonClient.start();

    debug(`enabled, ${this._rules.length} rule(s)`);
    const match = this._rules[0]?.match ?? {};
    debug(`rule=${this._rules[0]?.name} token="${match.titleExact}"`);

    this._connect(global.window_manager, 'map', (...args) => {
      // GJS passes the emitting object first, then signal arguments. Accept either shape
      // so this keeps working if that changes: pick whichever argument is the window actor.
      const actor = args.find((arg) => arg?.meta_window) ?? args[0];
      const win = actor?.meta_window;
      if (!win) return;
      this._register(win, actor);
      this._processWindow(win, 'map', actor);
    });

    this._scan(); // windows mapped before the extension loaded
    this._scanId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 10000, () => {
      this._scan();
      this._scanId = 0;
      return GLib.SOURCE_REMOVE;
    });
  }

  disable() {
    if (this._scanId) GLib.source_remove(this._scanId);
    this._scanId = 0;

    for (const id of this._pendingSources) {
      try {
        GLib.source_remove(id);
      } catch (e) {
        logError(`source_remove: ${e}`);
      }
    }
    this._pendingSources.clear();

    this._clearCallUI(true);
    for (const cancellable of this._actionCancellables) cancellable.cancel();
    this._actionCancellables.clear();

    if (this._daemonClient) {
      this._daemonClient.close();
      this._daemonClient = null;
    }

    for (const [obj, id] of this._signalIds) {
      try {
        obj.disconnect(id);
      } catch (e) {
        logError(`disconnect: ${e}`);
      }
    }
    this._signalIds = [];

    for (const win of this._touched) {
      try {
        win.unmake_above?.();
      } catch (e) {
        logError(`unmake_above: ${e}`);
      }
      try {
        if (win.is_on_all_workspaces?.()) {
          win.unstick?.();
        }
      } catch (e) {
        logError(`unstick: ${e}`);
      }
    }
    this._touched.clear();
    this._known.clear();
    this._rules = [];
    debug('disabled');
  }

  _connect(obj, signal, callback, quiet = false) {
    if (!obj) return 0;
    try {
      const id = obj.connect(signal, callback);
      this._signalIds.push([obj, id]);
      return id;
    } catch (e) {
      if (!quiet) logError(`connect ${signal}: ${e}`);
      return 0;
    }
  }

  _removeSignalId(obj, id) {
    const idx = this._signalIds.findIndex(([o, i]) => o === obj && i === id);
    if (idx !== -1) this._signalIds.splice(idx, 1);
  }

  _removeSignalsForObject(obj) {
    this._signalIds = this._signalIds.filter(([o]) => o !== obj);
  }

  _loadRules() {
    try {
      const [ok, bytes] = this.dir.get_child('rules.json').load_contents(null);
      if (!ok) return;
      const data = JSON.parse(new TextDecoder().decode(bytes));
      this._rules = (Array.isArray(data.rules) ? data.rules : []).map((rule) => {
        const match = { ...(rule.match || {}) };
        if (match.titleFile) {
          try {
            const [, token] = this.dir.get_child(match.titleFile).load_contents(null);
            match.titleExact = new TextDecoder().decode(token).trim();
          } catch (e) {
            logError(`${match.titleFile}: ${e}`);
          }
          delete match.titleFile;
        }
        return { ...rule, match };
      });
    } catch (e) {
      logError(`reading rules.json: ${e}`);
    }
  }

  _info(win) {
    return {
      title: win.get_title?.() ?? null,
      appId: win.get_app_id?.() ?? null,
      wmClass: win.get_wm_class?.() ?? null,
      role: win.get_role?.() ?? null,
    };
  }

  // Quiet: only registers windows that appeared before the extension loaded.
  _scan() {
    for (const actor of global.get_window_actors()) {
      const win = actor?.meta_window;
      if (!win || this._known.has(win)) continue;
      this._register(win, actor);
      this._processWindow(win, 'scan', actor);
    }
  }

  _register(win, actor) {
    if (this._known.has(win)) return;
    this._known.add(win);

    // Clean up signals when the window or its actor is unmanaged/destroyed.
    this._connect(
      win,
      'unmanaged',
      () => {
        this._known.delete(win);
        this._touched.delete(win);
        this._removeSignalsForObject(win);
      },
      true
    );

    if (actor) {
      this._connect(
        actor,
        'destroy',
        () => {
          this._removeSignalsForObject(actor);
        },
        true
      );
    }

    // The title is often set after mapping; wm-class/app-id may change later too.
    // Connecting to a notify signal for a property that does not exist is harmless.
    this._connect(win, 'notify::title', () => this._processWindow(win, 'notify::title', actor));
    this._connect(win, 'notify::app-id', () => this._processWindow(win, 'notify::app-id', actor));
    this._connect(win, 'notify::wm-class', () => this._processWindow(win, 'notify::wm-class', actor));

    // Mutter can move a window on its first frame, so place it once again afterwards.
    // Connecting to a nonexistent signal throws, which is how we detect its absence.
    let firstFrameId = 0;
    const onFirstFrame = () => {
      if (firstFrameId && actor) {
        try {
          actor.disconnect(firstFrameId);
        } catch (e) {}
        this._removeSignalId(actor, firstFrameId);
        firstFrameId = 0;
      }
      this._processWindow(win, 'first-frame', actor);
    };
    firstFrameId = this._connect(actor, 'first-frame', onFirstFrame, true);
    if (!firstFrameId) this._scheduleFirstFrame(win, actor);
  }

  _scheduleFirstFrame(win, actor) {
    const idleId = GLib.idle_add(GLib.PRIORITY_DEFAULT, () => {
      this._pendingSources.delete(idleId);
      if (this._known.has(win)) this._processWindow(win, 'idle', actor);
      return GLib.SOURCE_REMOVE;
    });
    this._pendingSources.add(idleId);

    const timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 200, () => {
      this._pendingSources.delete(timeoutId);
      if (this._known.has(win)) this._processWindow(win, 'timeout', actor);
      return GLib.SOURCE_REMOVE;
    });
    this._pendingSources.add(timeoutId);
  }

  _processWindow(win, reason, actor) {
    const info = this._info(win);
    const rule = this._rules.find((candidate) => this._matches(info, candidate.match));
    debug(`${reason} window rule match=${rule?.name ?? null}`);
    if (rule) this._apply(win, rule);
  }

  _apply(win, rule) {
    const actions = rule.actions || {};
    if (actions.alwaysOnTop) {
      try {
        if (!win.is_above?.() || !this._touched.has(win)) {
          win.make_above();
          this._touched.add(win);
        }
      } catch (e) {
        logError(`make_above: ${e}`);
      }
    }
    if (actions.sticky) {
      try {
        if (!win.is_on_all_workspaces?.() || !this._touched.has(win)) {
          win.stick?.();
          this._touched.add(win);
        }
      } catch (e) {
        logError(`stick: ${e}`);
      }
    }
    if (actions.position) this._place(win, actions.position, actions.size);
  }

  _onDaemonStatus(status) {
    try {
      const ownsCall = status.call_presenter === 'extension' &&
        status.call_presenter_id === this._daemonClient?._registeredId;
      if (!ownsCall || !status.call) {
        this._callOwner = false;
        this._call = null;
        this._collapsedCallKey = null;
        this._resetElapsed();
        this._clearCallUI(false);
        return;
      }

      const call = status.call;
      const sameCall = this._call && this._call.key === call.key;
      if (!sameCall || this._call.phase === 'ringing' && call.phase !== 'ringing') {
        this._elapsedBase = Number(call.elapsed) || 0;
        this._elapsedStart = GLib.get_monotonic_time();
        this._collapsedCallKey = null;
      } else if (call.phase !== 'ringing' && Number(call.elapsed) > this._elapsedSeconds()) {
        this._elapsedBase = Number(call.elapsed);
        this._elapsedStart = GLib.get_monotonic_time();
      }
      this._callOwner = true;
      this._call = call;
      this._renderCallUI();
    } catch (e) {
      logError('status handler failed');
      this._clearCallUI(false);
    }
  }

  _resetElapsed() {
    this._elapsedBase = 0;
    this._elapsedStart = 0;
  }

  _elapsedSeconds() {
    if (!this._elapsedStart) return this._elapsedBase;
    return this._elapsedBase + Math.floor((GLib.get_monotonic_time() - this._elapsedStart) / 1000000);
  }

  _renderCallUI() {
    if (!this._callOwner || !this._call) {
      this._clearCallUI(false);
      return;
    }
    const mode = this._call.phase === 'ringing' && this._collapsedCallKey !== this._call.key
      ? 'card'
      : 'pill';
    const signature = JSON.stringify([
      mode,
      this._call.key,
      this._call.phase,
      this._call.name,
      this._call.detail,
      this._call.answerLabel,
      this._call.endLabel,
    ]);
    if (signature === this._callViewSignature && (this._callCard || this._callPill)) return;
    this._removeCallActors();
    if (mode === 'card') {
      this._showCallCard();
    } else {
      this._showCallPill();
    }
    this._callViewSignature = signature;
    if (this._call.phase !== 'ringing') this._startCallTimer();
  }

  _showCallCard() {
    const card = new St.BoxLayout({
      vertical: true,
      style: 'background-color: #242424; color: white; border-radius: 12px; padding: 16px; spacing: 10px; min-width: 300px;',
      reactive: true,
    });
    const header = new St.BoxLayout({ style: 'spacing: 8px;' });
    const title = new St.Label({ text: this._call.name || 'Incoming call', x_expand: true });
    const collapse = new St.Button({ label: '×', style: 'min-width: 28px; min-height: 28px;', can_focus: true });
    collapse.connect('clicked', () => {
      try {
        this._collapsedCallKey = this._call?.key ?? null;
        this._renderCallUI();
      } catch (e) {
        logError('call card collapse failed');
      }
    });
    header.add_child(title);
    header.add_child(collapse);
    card.add_child(header);
    if (this._call.detail) card.add_child(new St.Label({ text: this._call.detail }));
    const buttons = new St.BoxLayout({ style: 'spacing: 8px;' });
    const answer = new St.Button({ label: this._call.answerLabel || 'Answer', style: 'background-color: #2e7d32; padding: 8px 14px; border-radius: 6px;' });
    const decline = new St.Button({ label: this._call.endLabel || 'Decline', style: 'background-color: #b3261e; padding: 8px 14px; border-radius: 6px;' });
    const key = this._call.key;
    answer.connect('clicked', () => this._sendCallAction('call_answer', key));
    decline.connect('clicked', () => this._sendCallAction('call_end', key));
    buttons.add_child(answer);
    buttons.add_child(decline);
    card.add_child(buttons);
    Main.layoutManager.addChrome(card, { trackFullscreen: false });
    this._callCard = card;
    this._positionCallCard();
    if (!this._callStageSignalId) {
      try {
        this._callStageSignalId = global.stage.connect('notify::allocation', () => this._positionCallCard());
      } catch (e) {
        this._callStageSignalId = 0;
      }
    }
  }

  _positionCallCard() {
    if (!this._callCard) return;
    try {
      const stageWidth = global.stage.width;
      const cardWidth = this._callCard.width || this._callCard.get_preferred_width(-1)[1];
      this._callCard.set_position(Math.max(0, stageWidth - cardWidth - 16), Main.panel.height + 12);
    } catch (e) {
      // Allocation can be unavailable during shell startup.
    }
  }

  _showCallPill() {
    const pill = new St.BoxLayout({ style: 'background-color: #242424; color: white; border-radius: 12px; padding: 4px 10px; spacing: 8px;', reactive: true });
    if (this._call.phase === 'ringing') {
      const label = new St.Label({ text: `Incoming call: ${this._call.name || 'Unknown'}` });
      pill.add_child(label);
      pill.connect('button-press-event', () => {
        try {
          this._collapsedCallKey = null;
          this._renderCallUI();
        } catch (e) {
          logError('call card restore failed');
        }
        return Clutter.EVENT_STOP;
      });
    } else {
      const label = new St.Label({ text: this._formatElapsed(this._elapsedSeconds()) });
      pill.add_child(label);
      const hangup = new St.Button({ label: this._call.endLabel || 'Hang Up', style: 'background-color: #b3261e; padding: 4px 10px; border-radius: 6px;' });
      const key = this._call.key;
      hangup.connect('clicked', () => this._sendCallAction('call_end', key));
      pill.add_child(hangup);
      this._callTimerLabel = label;
    }
    Main.panel._rightBox.add_child(pill);
    this._callPill = pill;
  }

  _formatElapsed(seconds) {
    const minutes = Math.floor(seconds / 60);
    const remainder = seconds % 60;
    return `${minutes}:${String(remainder).padStart(2, '0')}`;
  }

  _startCallTimer() {
    this._clearCallTimer();
    this._callTimerId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 1000, () => {
      try {
        if (this._callTimerLabel) this._callTimerLabel.text = this._formatElapsed(this._elapsedSeconds());
      } catch (e) {
        logError('call timer update failed');
      }
      return this._callTimerLabel ? GLib.SOURCE_CONTINUE : GLib.SOURCE_REMOVE;
    });
  }

  _clearCallTimer() {
    if (this._callTimerId) GLib.source_remove(this._callTimerId);
    this._callTimerId = 0;
    this._callTimerLabel = null;
  }

  _removeCallActors() {
    this._clearCallTimer();
    if (this._callCard) {
      try { Main.layoutManager.removeChrome(this._callCard); } catch (e) {}
      try { this._callCard.destroy(); } catch (e) {}
      this._callCard = null;
    }
    if (this._callPill) {
      try { Main.panel._rightBox.remove_child(this._callPill); } catch (e) {}
      try { this._callPill.destroy(); } catch (e) {}
      this._callPill = null;
    }
    if (this._callStageSignalId) {
      try { global.stage.disconnect(this._callStageSignalId); } catch (e) {}
      this._callStageSignalId = 0;
    }
    this._callViewSignature = null;
  }

  _clearCallUI(resetState) {
    this._removeCallActors();
    if (resetState) {
      this._call = null;
      this._callOwner = false;
      this._collapsedCallKey = null;
      this._resetElapsed();
    }
  }

  _sendCallAction(cmd, key) {
    const cancellable = new Gio.Cancellable();
    this._actionCancellables.add(cancellable);
    const finish = (connection) => {
      try { connection?.close(null); } catch (e) {}
      this._actionCancellables.delete(cancellable);
    };
    try {
      const client = new Gio.SocketClient();
      client.connect_async(
        Gio.UnixSocketAddress.new(`${GLib.get_user_runtime_dir()}/smartconnectd.sock`),
        cancellable,
        (source, result) => {
          let connection = null;
          try {
            connection = source.connect_finish(result);
            const bytes = new GLib.Bytes(new TextEncoder().encode(`${JSON.stringify({ cmd, key })}\n`));
            connection.get_output_stream().write_bytes_async(bytes, GLib.PRIORITY_DEFAULT, cancellable, (stream, writeResult) => {
              try { stream.write_bytes_finish(writeResult); } catch (e) {}
              finish(connection);
            });
          } catch (e) {
            finish(connection);
          }
        }
      );
    } catch (e) {
      finish(null);
    }
  }

  // Pure data match: no Meta.Window calls, so debug output shows exactly what was compared.
  _matches(info, match) {
    if (!match) return false;
    const has = (list, value) =>
      Array.isArray(list) &&
      list.length > 0 &&
      value !== null &&
      value !== undefined &&
      list.includes(value);
    if (match.appId && !has(match.appId, info.appId)) return false;
    if (match.wmClass && !has(match.wmClass, info.wmClass)) return false;
    if (match.role && !has(match.role, info.role)) return false;
    if (match.titleExact !== undefined && info.title !== match.titleExact) return false;
    return !!(match.appId || match.wmClass || match.role || match.titleExact !== undefined);
  }

  _place(win, position, size) {
    try {
      const monitor = global.display.get_monitor_geometry(win.get_monitor());
      const frame = win.get_frame_rect();
      const width = size?.width ?? frame.width;
      const height = size?.height ?? frame.height;
      const mx = position.marginX ?? 0;
      const my = position.marginY ?? 0;
      let x = monitor.x;
      let y = monitor.y;
      switch (position.corner) {
        case 'top-left':
          x = monitor.x + mx;
          y = monitor.y + my;
          break;
        case 'top-right':
          x = monitor.x + monitor.width - width - mx;
          y = monitor.y + my;
          break;
        case 'bottom-left':
          x = monitor.x + mx;
          y = monitor.y + monitor.height - height - my;
          break;
        case 'bottom-right':
          x = monitor.x + monitor.width - width - mx;
          y = monitor.y + monitor.height - height - my;
          break;
        default:
          return;
      }
      x = Math.round(x);
      y = Math.round(y);
      debug(`placing at ${x},${y} ${Math.round(width)}x${Math.round(height)}`);
      if (size) win.move_resize_frame(true, x, y, Math.round(width), Math.round(height));
      else win.move_frame(true, x, y);
    } catch (e) {
      logError(`placing window: ${e}`);
    }
  }
}
