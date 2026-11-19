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
// Debug logging is controlled by a `debug.txt` file in the extension directory containing `1`.
// Toggle it and disable/enable the extension; no shell restart needed. Errors are always logged.

import GLib from 'gi://GLib';
import { Extension } from 'resource:///org/gnome/shell/extensions/extension.js';

let DEBUG = false;

function debug(message) {
  if (DEBUG) log(`window-rules: ${message}`);
}

function logError(message) {
  log(`window-rules: ${message}`);
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
    this._loadRules();

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
      this._processWindow(win, 'map');
    });

    this._scan(); // windows mapped before the extension loaded
    this._scanId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 5000, () => {
      this._scan();
      return GLib.SOURCE_CONTINUE;
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
        win.unmake_above();
      } catch (e) {
        logError(`unmake_above: ${e}`);
      }
    }
    this._touched.clear();
    this._known.clear();
    this._rules = [];
    debug('disabled');
  }

  _connect(obj, signal, callback, quiet = false) {
    try {
      const id = obj.connect(signal, callback);
      this._signalIds.push([obj, id]);
      return id;
    } catch (e) {
      if (!quiet) logError(`connect ${signal}: ${e}`);
      return 0;
    }
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

  _describe(info) {
    return `title="${info.title}" appId="${info.appId}" wmClass="${info.wmClass}" role="${info.role}"`;
  }

  // Quiet: only registers windows that appeared before the extension loaded.
  _scan() {
    for (const actor of global.get_window_actors()) {
      const win = actor?.meta_window;
      if (!win || this._known.has(win)) continue;
      this._register(win, actor);
      this._processWindow(win, 'scan');
    }
  }

  _register(win, actor) {
    if (this._known.has(win)) return;
    this._known.add(win);

    // The title is often set after mapping; wm-class/app-id may change later too.
    // Connecting to a notify signal for a property that does not exist is harmless.
    this._connect(win, 'notify::title', () => this._processWindow(win, 'notify::title'));
    this._connect(win, 'notify::app-id', () => this._processWindow(win, 'notify::app-id'));
    this._connect(win, 'notify::wm-class', () => this._processWindow(win, 'notify::wm-class'));

    // Mutter can move a window on its first frame, so place it once again afterwards.
    // Connecting to a nonexistent signal throws, which is how we detect its absence.
    const id = this._connect(
      actor,
      'first-frame',
      () => this._processWindow(win, 'first-frame'),
      true
    );
    if (!id) this._scheduleFirstFrame(win);
  }

  _scheduleFirstFrame(win) {
    const idleId = GLib.idle_add(GLib.PRIORITY_DEFAULT, () => {
      this._pendingSources.delete(idleId);
      if (this._known.has(win)) this._processWindow(win, 'idle');
      return GLib.SOURCE_REMOVE;
    });
    this._pendingSources.add(idleId);

    const timeoutId = GLib.timeout_add(GLib.PRIORITY_DEFAULT, 200, () => {
      this._pendingSources.delete(timeoutId);
      if (this._known.has(win)) this._processWindow(win, 'timeout');
      return GLib.SOURCE_REMOVE;
    });
    this._pendingSources.add(timeoutId);
  }

  _processWindow(win, reason) {
    const info = this._info(win);
    const rule = this._rules.find((candidate) => this._matches(info, candidate.match));
    debug(`${reason} ${this._describe(info)} match=${rule?.name ?? null}`);
    if (rule) this._apply(win, rule);
  }

  _apply(win, rule) {
    const actions = rule.actions || {};
    if (actions.alwaysOnTop && !this._touched.has(win)) {
      try {
        win.make_above();
        this._touched.add(win);
      } catch (e) {
        logError(`make_above: ${e}`);
      }
    }
    if (actions.position) this._place(win, actions.position, actions.size);
  }

  // Pure data match: no Meta.Window calls, so debug output shows exactly what was compared.
  _matches(info, match) {
    if (!match) return false;
    const has = (list, value) =>
      Array.isArray(list) && list.length > 0 && value !== null && value !== undefined && list.includes(value);
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
