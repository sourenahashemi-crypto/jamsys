/* JamSys instrument cluster as a standalone window.
 *
 * Why this exists, three reasons:
 *
 *  1. GNOME Shell caches a loaded extension module. A newly installed or newly fixed
 *     extension does not take effect until the Shell restarts, which under Wayland
 *     means logging out — verified: the Shell does not even discover a new extension
 *     directory until then. This window shows the cluster immediately.
 *  2. The Shell extension only works on GNOME. This works on any desktop that can run
 *     a GTK4 window.
 *  3. It is the fastest way to iterate on the drawing, because it is live rather than
 *     a PNG.
 *
 * It lives beside the extension because it shares `gauges.js` verbatim — one copy of
 * the rendering, three consumers (Shell widget, this window, the PNG harness).
 *
 * Resizing: an undecorated window on Wayland has no resize edges, so the gadget is
 * resized by scrolling over it, by keyboard, or from its own menu, and the chosen size
 * is remembered.
 *
 * Staying on top: an ordinary Wayland window cannot raise itself above others — that
 * is a deliberate compositor decision, not an oversight here. On GNOME, `Alt+Space`
 * opens the window menu, which has "Always on Top", and that works even undecorated.
 * The Shell extension has no such limitation, which is why it remains the primary
 * surface.
 */

import Gtk from 'gi://Gtk?version=4.0';
import Gdk from 'gi://Gdk?version=4.0';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import {drawCluster, drawClusterBare, bareHitRegions,
        CLUSTER_W, CLUSTER_H, BARE_W, BARE_H} from './gauges.js';

const Cairo = imports.cairo;

// Only present when GTK has an X11 backend. Its absence is not an error; it just
// means the stacking toggles cannot work in this session.
let GdkX11 = null;
try {
    GdkX11 = (await import('gi://GdkX11?version=4.0')).default;
} catch {
    GdkX11 = null;
}

const BUS_NAME = 'org.jamsys.Daemon';
const OBJECT_PATH = '/org/jamsys/Daemon';

const Iface = `
<node>
  <interface name="org.jamsys.Daemon">
    <method name="GetState"><arg type="s" direction="out" name="state"/></method>
    <signal name="StateChanged"><arg type="s" name="state"/></signal>
  </interface>
</node>`;
const Proxy = Gio.DBusProxy.makeProxyWrapper(Iface);

const MIN_SCALE = 0.6;
const MAX_SCALE = 4.0;
/** The size the gadget returns to with `0` or "Reset size". */
const DEFAULT_SCALE = 1.30;

/* ------------------------------------------------------- persisted settings */

const CONF_DIR = GLib.build_filenamev([GLib.get_user_config_dir(), 'jamsys']);
const CONF = GLib.build_filenamev([CONF_DIR, 'cluster.json']);

function loadConf() {
    try {
        const [ok, bytes] = GLib.file_get_contents(CONF);
        if (ok) return JSON.parse(new TextDecoder().decode(bytes));
    } catch { /* first run, or unreadable — defaults are fine */ }
    return {};
}

function saveConf(c) {
    try {
        GLib.mkdir_with_parents(CONF_DIR, 0o700);
        GLib.file_set_contents(CONF, JSON.stringify(c, null, 2));
    } catch (e) {
        printerr(`jamsys-cluster: could not save settings: ${e.message}`);
    }
}

/* ---------------------------------------------------------------- arguments */

function parseArgs(argv) {
    const saved = loadConf();
    // A default of 1.3 rather than 1.0: at 1.0 the auxiliary dials are legible but
    // the secondary readings are not, on a high-density laptop panel.
    const o = {
        scale: saved.scale ?? DEFAULT_SCALE,
        opacity: saved.opacity ?? 0.92,
        decorated: saved.decorated ?? false,
        bare: saved.bare ?? true,
        ontop: saved.ontop ?? false,
        sticky: saved.sticky ?? false,
    };
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        if (a === '--scale') o.scale = parseFloat(argv[++i]) || o.scale;
        else if (a === '--opacity') o.opacity = parseFloat(argv[++i]) || o.opacity;
        else if (a === '--decorated') o.decorated = true;
        else if (a === '--undecorated') o.decorated = false;
        else if (a === '--bare') o.bare = true;
        else if (a === '--housing') o.bare = false;
        else if (a === '--on-top') o.ontop = true;
        else if (a === '--no-on-top') o.ontop = false;
        else if (a === '--all-workspaces') o.sticky = true;
        else if (a === '--reset') {
            o.scale = DEFAULT_SCALE; o.opacity = 0.92; o.decorated = false;
            o.bare = true; o.ontop = false; o.sticky = false;
        }
        else if (a === '--help' || a === '-h') o.help = true;
    }
    o.scale = Math.max(MIN_SCALE, Math.min(MAX_SCALE, o.scale));
    o.opacity = Math.max(0.2, Math.min(1.0, o.opacity));
    return o;
}

const opts = parseArgs(ARGV ?? []);
if (opts.help) {
    print(`jamsys-cluster — the JamSys instrument cluster in its own window

USAGE:
  jamsys-cluster [--scale N] [--opacity N] [--decorated] [--on-top]
                 [--all-workspaces] [--reset]

  --scale N       ${MIN_SCALE} to ${MAX_SCALE}; 1.0 is ${CLUSTER_W}x${CLUSTER_H} px. Default 1.3.
  --opacity N     0.2 to 1.0 of the housing. The instruments stay opaque.
  --decorated     keep a title bar
  --on-top        keep the cluster above other windows (needs XWayland)
  --all-workspaces  show it on every workspace
  --reset         forget the remembered size, opacity and stacking

WHILE IT IS OPEN
  scroll              resize
  + / -               resize
  0                   reset to the default size
  right-click         menu: sizes, opacity, title bar, always-on-top
  left-click          open the full JamSys window
  Escape, Ctrl+Q      close

KEEPING IT ABOVE OTHER WINDOWS
  Right-click and tick "Always on top". This asks the window manager over X11, so
  the cluster runs on XWayland by default; the launcher handles that. Wayland
  itself gives an application no way to raise itself, so under a native Wayland
  surface the toggle is greyed out.

Size, opacity and stacking are remembered in ${CONF}`);
    imports.system.exit(0);
}

/* --------------------------------------------------------------------- app */

const app = new Gtk.Application({
    application_id: 'org.jamsys.Cluster',
    flags: Gio.ApplicationFlags.NON_UNIQUE,
});

let state = null;
let area = null;
let win = null;
let proxy = null;
let scale = opts.scale;
let opacity = opts.opacity;
let decorated = opts.decorated;
let bare = opts.bare;
let ontop = opts.ontop;
let sticky = opts.sticky;
let saveTimer = 0;

const natW = () => (bare ? BARE_W : CLUSTER_W);
const natH = () => (bare ? BARE_H : CLUSTER_H);
const sizeFor = s => [Math.round(natW() * s), Math.round(natH() * s)];

function applySize() {
    const [w, h] = sizeFor(scale);
    area.set_content_width(w);
    area.set_content_height(h);
    // A resizable window keeps whatever the user dragged it to, so shrink it back to
    // the requested size explicitly rather than only setting the content hint.
    win.set_default_size(w, h);
    if (!decorated)
        win.set_size_request(-1, -1);
    area.queue_draw();
    // The shape follows the size, so re-cut it once the new size has been applied.
    GLib.idle_add(GLib.PRIORITY_DEFAULT_IDLE, () => {
        applyInputShape();
        return false;
    });
    scheduleSave();
}

function setScale(s) {
    scale = Math.max(MIN_SCALE, Math.min(MAX_SCALE, s));
    applySize();
}

/** Debounced, so a scroll gesture does not write the file on every tick. */
function scheduleSave() {
    if (saveTimer) GLib.Source.remove(saveTimer);
    saveTimer = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 1, () => {
        saveTimer = 0;
        saveConf({scale, opacity, decorated, bare, ontop, sticky});
        return false;
    });
}

app.connect('activate', () => {
    win = new Gtk.ApplicationWindow({
        application: app,
        title: 'JamSys',
        decorated,
        // Resizable so the window manager offers edges when decorated, and so the
        // drawing can fill whatever the user drags it to.
        resizable: true,
    });

    // The cluster paints its own rounded housing, so everything behind it must be
    // transparent or the corners sit on a grey square.
    const css = new Gtk.CssProvider();
    css.load_from_data(
        'window, window.background { background: transparent; box-shadow: none; }', -1);
    Gtk.StyleContext.add_provider_for_display(
        Gdk.Display.get_default(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION);

    area = new Gtk.DrawingArea({hexpand: true, vexpand: true});
    area.set_draw_func((_a, cr, w, h) => {
        try {
            if (!state) {
                cr.setSourceRGBA(0.03, 0.04, 0.05, 0.75);
                cr.paint();
                cr.selectFontFace('Ubuntu Sans Mono', 0, 1);
                cr.setFontSize(12);
                cr.setSourceRGBA(0.55, 0.60, 0.65, 1);
                const msg = 'waiting for jamsysd …';
                const e = cr.textExtents(msg);
                cr.moveTo(w / 2 - e.width / 2, h / 2);
                cr.showText(msg);
                cr.newPath();
                return;
            }
            if (bare) drawClusterBare(cr, w, h, state, {opacity});
            else drawCluster(cr, w, h, state, {opacity});
        } catch (e) {
            printerr(`jamsys-cluster: draw failed: ${e.message}`);
        }
    });

    buildMenu();

    // The whole face drags the window, since there is usually no title bar to grab.
    const handle = new Gtk.WindowHandle({child: area});
    win.set_child(handle);

    // Left click opens the full window on whatever is wrong.
    const click = new Gtk.GestureClick({button: 1});
    click.connect('released', () => openApp());
    area.add_controller(click);

    // Right click opens the gadget's own menu.
    const rclick = new Gtk.GestureClick({button: 3});
    rclick.connect('pressed', (_g, _n, x, y) => {
        popover.set_pointing_to(new Gdk.Rectangle({x, y, width: 1, height: 1}));
        popover.popup();
    });
    area.add_controller(rclick);

    // Scroll resizes. This is how the gadget is resized without decorations, and it
    // is the gesture people already try on a desktop widget.
    installFreeTransform(area);

    const scroll = new Gtk.EventControllerScroll({
        flags: Gtk.EventControllerScrollFlags.VERTICAL,
    });
    scroll.connect('scroll', (_c, _dx, dy) => {
        setScale(scale * (dy < 0 ? 1.08 : 1 / 1.08));
        return true;
    });
    area.add_controller(scroll);

    const keys = new Gtk.EventControllerKey();
    keys.connect('key-pressed', (_c, keyval, _code, mods) => {
        const ctrl = (mods & Gdk.ModifierType.CONTROL_MASK) !== 0;
        switch (keyval) {
        case Gdk.KEY_Escape:                       win.close(); return true;
        case Gdk.KEY_q: case Gdk.KEY_Q:            if (ctrl) { win.close(); return true; } break;
        case Gdk.KEY_plus: case Gdk.KEY_equal:
        case Gdk.KEY_KP_Add:                       setScale(scale * 1.12); return true;
        case Gdk.KEY_minus: case Gdk.KEY_KP_Subtract:
                                                   setScale(scale / 1.12); return true;
        case Gdk.KEY_0: case Gdk.KEY_KP_0:         setScale(DEFAULT_SCALE); return true;
        }
        return false;
    });
    win.add_controller(keys);

    applySize();

    win.connect('map', () => {
        const ok = stackingAvailable();
        for (const a of [ontopAction, stickyAction]) a?.set_enabled(ok);
        ontopAction?.set_state(GLib.Variant.new_boolean(ontop && ok));
        stickyAction?.set_state(GLib.Variant.new_boolean(sticky && ok));
        // Mutter can ignore state set at the instant of mapping, so ask once the
        // window is actually on screen.
        GLib.timeout_add(GLib.PRIORITY_DEFAULT, 120, () => {
            applyStacking();
            applyInputShape();
            return false;
        });
    });

    win.present();
    connectDaemon();
});


/* ---------------------------------------------------------------- stacking */
/* Wayland has no protocol for a client to raise itself, and GTK4 dropped
 * set_keep_above. The one route that works on GNOME is EWMH on an XWayland
 * window: Mutter honours _NET_WM_STATE_ABOVE and _NET_WM_STATE_STICKY for X11
 * clients. Measured on GNOME Shell 50.1: with ABOVE set, the cluster stays on
 * top even when another window is raised afterwards.
 *
 * Changing the state needs a ClientMessage to the root window, which nothing
 * introspectable exposes, so it goes through the jamsys-xabove sidecar. */

/** The X window id, or null when this is a native Wayland surface. */
function xid() {
    if (!GdkX11) return null;
    const surface = win?.get_surface();
    if (!surface || !(surface instanceof GdkX11.X11Surface)) return null;
    try {
        return surface.get_xid();
    } catch {
        return null;
    }
}

/** True when the stacking toggles can do anything in this session. */
function stackingAvailable() {
    return xid() !== null;
}

function helperPath() {
    const here = GLib.path_get_dirname(import.meta.url.replace('file://', ''));
    const candidates = [
        GLib.build_filenamev([GLib.get_home_dir(), '.local', 'bin', 'jamsys-xabove']),
        '/usr/bin/jamsys-xabove',
        GLib.build_filenamev([here, '..', '..', 'jamsys-ui', 'bin', 'jamsys-xabove']),
    ];
    for (const c of candidates) {
        if (GLib.file_test(c, GLib.FileTest.IS_EXECUTABLE)) return c;
    }
    return GLib.find_program_in_path('jamsys-xabove');
}

/** Ask the window manager to add or remove one stacking state. */
function setWmState(name, on) {
    const id = xid();
    if (id === null) return false;
    const helper = helperPath();
    if (!helper) {
        printerr('jamsys-cluster: jamsys-xabove not found; cannot change stacking');
        return false;
    }
    try {
        const proc = Gio.Subprocess.new(
            [helper, name, String(id), on ? 'on' : 'off'],
            Gio.SubprocessFlags.STDERR_PIPE);
        proc.communicate_utf8_async(null, null, (pr, res) => {
            try {
                const [, , err] = pr.communicate_utf8_finish(res);
                if (!pr.get_successful())
                    printerr(`jamsys-cluster: ${name}: ${(err ?? '').trim()}`);
            } catch { /* the process is gone; nothing useful to report */ }
        });
        return true;
    } catch (e) {
        printerr(`jamsys-cluster: could not run jamsys-xabove: ${e.message}`);
        return false;
    }
}

/** Push the remembered stacking preferences at the window manager. */
function applyStacking() {
    if (!stackingAvailable()) return;
    setWmState('above', ontop);
    setWmState('sticky', sticky);
}

/* ---------------------------------------------------------- free transform */
/* Ctrl+drag anywhere on the gadget scales it continuously. This is the "free
 * transform" the presets were standing in for: no ladder of fixed sizes, just drag
 * until it looks right.
 *
 * It runs in the capture phase because the whole face is a Gtk.WindowHandle, which
 * would otherwise claim the drag for moving the window before it is seen here. */

let dragStartScale = 1;

function installFreeTransform(widget) {
    const drag = new Gtk.GestureDrag();
    drag.set_propagation_phase(Gtk.PropagationPhase.CAPTURE);
    drag.connect('drag-begin', g => {
        const ctrl = (g.get_current_event_state() & Gdk.ModifierType.CONTROL_MASK) !== 0;
        if (!ctrl) {
            // Not ours: let the window handle move the window instead.
            g.set_state(Gtk.EventSequenceState.DENIED);
            return;
        }
        dragStartScale = scale;
        g.set_state(Gtk.EventSequenceState.CLAIMED);
    });
    drag.connect('drag-update', (g, dx, dy) => {
        if (g.get_sequence_state(g.get_current_sequence()) !== Gtk.EventSequenceState.CLAIMED)
            return;
        // Diagonal distance, signed: right/down grows, left/up shrinks. Divided by
        // the natural width so the gesture feels the same at every size.
        const delta = (dx + dy) / 2 / natW();
        setScale(dragStartScale * (1 + delta * 2.4));
    });
    widget.add_controller(drag);
}

/* -------------------------------------------------------- input shaping */
/* With no housing there is no rectangle to click: the gaps between the dials
 * belong to whatever is behind the gadget. Without this, an invisible box would
 * swallow clicks meant for the window underneath, which is exactly the complaint
 * people have about desktop widgets. */

function applyInputShape() {
    const surface = win?.get_surface();
    if (!surface || typeof surface.set_input_region !== 'function') return;
    try {
        if (!bare || decorated) {
            // Rectangular again: hand back a null region so the whole window is live.
            surface.set_input_region(null);
            return;
        }
        const w = area.get_width(), h = area.get_height();
        if (w <= 0 || h <= 0) return;
        const region = new Cairo.Region();
        for (const r of bareHitRegions(w, h))
            region.unionRectangle({x: r.x, y: r.y, width: r.w, height: r.h});
        surface.set_input_region(region);
    } catch (e) {
        // Shaping is a refinement, never a requirement.
        printerr(`jamsys-cluster: could not shape the input region: ${e.message}`);
    }
}

/* -------------------------------------------------------------------- menu */

let popover = null;
let ontopAction = null;
let bareAction = null;
let stickyAction = null;

function buildMenu() {
    const menu = new Gio.Menu();

    const sizes = new Gio.Menu();
    sizes.append('Bigger', 'win.size::up');
    sizes.append('Smaller', 'win.size::down');
    sizes.append('Reset size', 'win.size::reset');
    menu.append_section('Size  (scroll, or Ctrl+drag)', sizes);

    const look = new Gio.Menu();
    look.append('More opaque', 'win.opacity::up');
    look.append('More transparent', 'win.opacity::down');
    look.append('Cut-out dials (no panel)', 'win.bare');
    look.append('Title bar', 'win.decorations');
    menu.append_section('Appearance', look);

    const stack = new Gio.Menu();
    stack.append('Always on top', 'win.ontop');
    stack.append('On all workspaces', 'win.sticky');
    menu.append_section('Stacking', stack);

    const act = new Gio.Menu();
    act.append('Open JamSys', 'win.open');
    act.append('Close', 'win.quit');
    menu.append_section(null, act);

    const add = (name, paramType, fn) => {
        const a = new Gio.SimpleAction({name, parameter_type: paramType});
        a.connect('activate', (_a, p) => fn(p));
        win.add_action(a);
    };
    add('size', GLib.VariantType.new('s'), p => {
        const which = p.get_string()[0];
        if (which === 'up') setScale(scale * 1.15);
        else if (which === 'down') setScale(scale / 1.15);
        else setScale(DEFAULT_SCALE);
    });
    add('opacity', GLib.VariantType.new('s'), p => {
        opacity = Math.max(0.2, Math.min(1.0,
            opacity + (p.get_string()[0] === 'up' ? 0.06 : -0.06)));
        area.queue_draw();
        scheduleSave();
    });
    add('decorations', null, () => {
        decorated = !decorated;
        win.set_decorated(decorated);
        applyInputShape();
        scheduleSave();
    });
    add('open', null, () => openApp());
    add('quit', null, () => win.close());

    bareAction = Gio.SimpleAction.new_stateful('bare', null, GLib.Variant.new_boolean(bare));
    bareAction.connect('activate', () => {
        bare = !bare;
        bareAction.set_state(GLib.Variant.new_boolean(bare));
        // The two modes have different natural aspects, so resize as well as redraw.
        applySize();
        applyInputShape();
        scheduleSave();
    });
    win.add_action(bareAction);

    // Stateful, so they draw as checkboxes and show what is currently in force.
    const toggle = (name, get, set) => {
        const a = Gio.SimpleAction.new_stateful(
            name, null, GLib.Variant.new_boolean(get()));
        a.connect('activate', () => {
            const next = !get();
            if (!stackingAvailable()) {
                showStackingUnavailable();
                return;
            }
            set(next);
            a.set_state(GLib.Variant.new_boolean(get()));
            scheduleSave();
        });
        // Greyed out rather than silently doing nothing on a native Wayland surface.
        a.set_enabled(stackingAvailable());
        win.add_action(a);
        return a;
    };
    ontopAction = toggle('ontop', () => ontop, v => { ontop = v; setWmState('above', v); });
    stickyAction = toggle('sticky', () => sticky, v => { sticky = v; setWmState('sticky', v); });

    popover = new Gtk.PopoverMenu({menu_model: menu, has_arrow: false});
    popover.set_parent(area);
}

function showStackingUnavailable() {
    const d = new Gtk.MessageDialog({
        transient_for: win,
        modal: true,
        text: 'Cannot change stacking in this session',
        secondary_text:
            'Always-on-top works by asking the window manager through X11, which ' +
            'needs the cluster to run on XWayland. This window is a native Wayland ' +
            'surface, and Wayland gives an application no way to raise itself.\n\n' +
            'Start it with the launcher, which uses XWayland by default:\n\n' +
            '    jamsys-cluster\n\n' +
            'Or, for this window only, press Alt+Space and choose “Always on Top”.',
        buttons: Gtk.ButtonsType.CLOSE,
    });
    d.connect('response', () => d.destroy());
    d.present();
}

function openApp() {
    const page = state?.alert_subsystem;
    const argv = page ? ['jamsys', '--page', page] : ['jamsys'];
    try {
        Gio.Subprocess.new(argv, Gio.SubprocessFlags.NONE);
    } catch (e) {
        printerr(`jamsys-cluster: could not launch jamsys: ${e.message}`);
    }
}

/* ------------------------------------------------------------------ daemon */

function connectDaemon() {
    try {
        proxy = new Proxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH);
    } catch (e) {
        printerr(`jamsys-cluster: cannot reach ${BUS_NAME}: ${e.message}`);
        printerr('  is the daemon running?  systemctl --user status jamsysd');
        GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 3, () => {
            connectDaemon();
            return false;
        });
        return;
    }
    proxy.connectSignal('StateChanged', (_p, _s, [json]) => apply(json));
    proxy.GetStateRemote(([json], err) => {
        if (err) {
            printerr(`jamsys-cluster: GetState failed: ${err.message ?? err}`);
            return;
        }
        apply(json);
    });
}

function apply(json) {
    try {
        state = JSON.parse(json);
    } catch (e) {
        printerr(`jamsys-cluster: bad state payload: ${e.message}`);
        return;
    }
    area?.queue_draw();
}

app.run([]);
