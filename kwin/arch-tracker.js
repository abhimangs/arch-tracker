// arch-tracker KWin script: reports window state to the arch-tracker daemon over D-Bus.
// Loaded at runtime by the daemon (org.kde.kwin.Scripting.loadScript); nothing is written to kwinrc.

var SERVICE = "dev.archtracker.Daemon";
var PATH = "/dev/archtracker/KWin";
var IFACE = "dev.archtracker.KWin1";

function relevant(w) {
    return w && w.managed && !w.deleted && !w.popupWindow && !w.tooltip && !w.notification &&
        !w.criticalNotification && !w.onScreenDisplay && !w.dndIcon && !w.outline && !w.inputMethod;
}

function info(w) {
    var g = w.frameGeometry;
    return {
        id: String(w.internalId),
        pid: w.pid || 0,
        cls: String(w.resourceClass || ""),
        name: String(w.resourceName || ""),
        df: String(w.desktopFileName || ""),
        cap: String(w.caption || ""),
        min: !!w.minimized,
        hidden: !!w.hidden,
        x: Math.round(g.x), y: Math.round(g.y), w: Math.round(g.width), h: Math.round(g.height),
        desks: (w.desktops || []).map(function (d) { return String(d.id); }),
        all_desks: !!w.onAllDesktops,
        acts: (w.activities || []).map(function (a) { return String(a); }),
        out: w.output ? String(w.output.name) : "",
        fs: !!w.fullScreen,
        skip: !!w.skipTaskbar,
        normal: !!w.normalWindow,
        dialog: !!w.dialog,
        utility: !!w.utility,
        special: !!w.specialWindow,
        dock: !!w.dock,
        desktop_win: !!w.desktopWindow,
        tf: w.transientFor ? String(w.transientFor.internalId) : null
    };
}

function send() {
    var wins = [];
    var so = workspace.stackingOrder;
    for (var i = 0; i < so.length; i++) {
        if (relevant(so[i])) wins.push(info(so[i]));
    }
    var a = workspace.activeWindow;
    var snap = {
        active: a ? String(a.internalId) : null,
        desktop: workspace.currentDesktop ? String(workspace.currentDesktop.id) : "",
        activity: String(workspace.currentActivity || ""),
        screens: (workspace.screens || []).map(function (s) {
            var g = s.geometry;
            return { name: String(s.name), x: g.x, y: g.y, w: g.width, h: g.height };
        }),
        windows: wins
    };
    callDBus(SERVICE, PATH, IFACE, "Snapshot", JSON.stringify(snap));
}

var timer = new QTimer();
timer.singleShot = true;
timer.interval = 150;
timer.timeout.connect(send);

function schedule() {
    if (!timer.active) timer.start();
}

function watch(w) {
    if (!w) return;
    w.captionChanged.connect(schedule);
    w.minimizedChanged.connect(schedule);
    w.desktopsChanged.connect(schedule);
    w.activitiesChanged.connect(schedule);
    w.outputChanged.connect(schedule);
    w.fullScreenChanged.connect(schedule);
    w.hiddenChanged.connect(schedule);
    w.stackingOrderChanged.connect(schedule);
    w.desktopFileNameChanged.connect(schedule);
    w.windowClassChanged.connect(schedule);
    w.interactiveMoveResizeFinished.connect(schedule);
    w.frameGeometryChanged.connect(function () {
        if (!(w.move || w.resize)) schedule();
    });
}

workspace.windowList().forEach(watch);
workspace.windowAdded.connect(function (w) { watch(w); schedule(); });
workspace.windowRemoved.connect(schedule);
workspace.windowActivated.connect(send);
workspace.currentDesktopChanged.connect(schedule);
workspace.currentActivityChanged.connect(schedule);
workspace.screensChanged.connect(schedule);

// Re-sync every minute so a restarted daemon is never stale for long.
var keepalive = new QTimer();
keepalive.interval = 60000;
keepalive.timeout.connect(send);
keepalive.start();

send();
