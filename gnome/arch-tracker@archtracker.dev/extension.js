// arch-tracker GNOME Shell extension: tells the arch-tracker daemon which windows there are, where,
// and which one has focus, the same snapshot its KWin script sends on Plasma (over D-Bus, on this
// computer only). It also raises a window when the daemon asks (a click on an agent's notice).

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import Meta from 'gi://Meta';
import Shell from 'gi://Shell';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

const SERVICE = 'dev.archtracker.Daemon';
const PATH = '/dev/archtracker/KWin';
const IFACE = 'dev.archtracker.KWin1';
const OWN_PATH = '/dev/archtracker/Gnome';
const OWN_XML = `<node>
  <interface name="dev.archtracker.Gnome1">
    <method name="Activate">
      <arg type="au" direction="in" name="pids"/>
      <arg type="s" direction="in" name="caption"/>
      <arg type="b" direction="out" name="found"/>
    </method>
  </interface>
</node>`;

const SETTLE_MS = 150;
const KEEPALIVE_S = 60;

function windows() {
    const all = global.display.list_all_windows().filter(w => !w.is_override_redirect());
    return global.display.sort_windows_by_stacking(all);
}

function info(w, tracker) {
    const r = w.get_frame_rect();
    const ws = w.get_workspace();
    const type = w.get_window_type();
    const parent = w.get_transient_for();
    const app = tracker.get_window_app(w);
    return {
        id: String(w.get_id()),
        pid: w.get_pid() || 0,
        cls: w.get_wm_class() || '',
        df: app ? app.get_id().replace(/\.desktop$/, '') : '',
        cap: w.get_title() || '',
        min: w.minimized,
        hidden: !w.minimized && !w.showing_on_its_workspace(),
        x: r.x, y: r.y, w: r.width, h: r.height,
        desks: ws ? [String(ws.index())] : [],
        all_desks: w.is_on_all_workspaces(),
        acts: [],
        out: String(w.get_monitor()),
        fs: w.is_fullscreen(),
        skip: w.is_skip_taskbar(),
        normal: type === Meta.WindowType.NORMAL,
        dialog: type === Meta.WindowType.DIALOG || type === Meta.WindowType.MODAL_DIALOG,
        utility: type === Meta.WindowType.UTILITY,
        special: false,
        dock: type === Meta.WindowType.DOCK,
        desktop_win: type === Meta.WindowType.DESKTOP,
        tf: parent ? String(parent.get_id()) : null,
    };
}

export default class ArchTracker extends Extension {
    enable() {
        this._tracker = Shell.WindowTracker.get_default();
        this._signals = [];
        this._windowSignals = new Map();
        this._settle = 0;
        const on = (obj, name, fn) => this._signals.push([obj, obj.connect(name, fn)]);
        const later = () => this._schedule();
        on(global.display, 'notify::focus-window', () => this._send());
        on(global.display, 'window-created', (_d, w) => { this._watch(w); later(); });
        on(global.display, 'restacked', later);
        on(global.display, 'in-fullscreen-changed', later);
        on(global.workspace_manager, 'active-workspace-changed', later);
        on(Main.layoutManager, 'monitors-changed', later);
        for (const w of global.display.list_all_windows())
            this._watch(w);
        this._keepalive = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, KEEPALIVE_S, () => {
            this._send();
            return GLib.SOURCE_CONTINUE;
        });
        this._dbus = Gio.DBusExportedObject.wrapJSObject(OWN_XML, this);
        this._dbus.export(Gio.DBus.session, OWN_PATH);
        this._send();
    }

    disable() {
        for (const [obj, id] of this._signals)
            obj.disconnect(id);
        for (const [w, ids] of this._windowSignals)
            ids.forEach(id => w.disconnect(id));
        this._signals = [];
        this._windowSignals.clear();
        if (this._settle)
            GLib.source_remove(this._settle);
        if (this._keepalive)
            GLib.source_remove(this._keepalive);
        this._settle = this._keepalive = 0;
        this._dbus?.unexport();
        this._dbus = null;
        this._tracker = null;
    }

    _watch(w) {
        if (!w || this._windowSignals.has(w))
            return;
        const later = () => this._schedule();
        const ids = ['notify::title', 'notify::minimized', 'notify::wm-class', 'workspace-changed',
            'position-changed', 'size-changed', 'notify::fullscreen'].map(s => w.connect(s, later));
        ids.push(w.connect('unmanaged', () => {
            this._windowSignals.get(w)?.forEach(id => w.disconnect(id));
            this._windowSignals.delete(w);
            later();
        }));
        this._windowSignals.set(w, ids);
    }

    _schedule() {
        if (this._settle)
            return;
        this._settle = GLib.timeout_add(GLib.PRIORITY_DEFAULT, SETTLE_MS, () => {
            this._settle = 0;
            this._send();
            return GLib.SOURCE_REMOVE;
        });
    }

    _send() {
        const focus = global.display.focus_window;
        const monitors = [];
        for (let i = 0; i < global.display.get_n_monitors(); i++) {
            const g = global.display.get_monitor_geometry(i);
            monitors.push({name: String(i), x: g.x, y: g.y, w: g.width, h: g.height});
        }
        const snap = {
            active: focus ? String(focus.get_id()) : null,
            desktop: String(global.workspace_manager.get_active_workspace_index()),
            activity: '',
            screens: monitors,
            windows: windows().map(w => info(w, this._tracker)),
        };
        Gio.DBus.session.call(SERVICE, PATH, IFACE, 'Snapshot',
            new GLib.Variant('(s)', [JSON.stringify(snap)]), null,
            Gio.DBusCallFlags.NO_AUTO_START, 2000, null, (conn, res) => {
                try {
                    conn.call_finish(res);
                } catch (e) {
                    // The daemon isn't running: the next change or the keepalive tries again.
                }
            });
    }

    // D-Bus: raise the window of the nearest of pids (a process, then its parents), titled
    // caption if one is; switches workspace if it has to.
    Activate(pids, caption) {
        let best = null, rank = Infinity;
        for (const w of windows()) {
            const i = pids.indexOf(w.get_pid());
            if (i < 0 || w.get_window_type() !== Meta.WindowType.NORMAL)
                continue;
            const r = i * 2 + (caption && w.get_title() !== caption ? 1 : 0);
            if (r < rank) {
                best = w;
                rank = r;
            }
        }
        if (best)
            Main.activateWindow(best);
        return best !== null;
    }
}
