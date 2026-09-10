/* JamSys instrument cluster as a standalone window.
 *
 * Why this exists, three reasons:
 *
 *  1. GNOME Shell caches a loaded extension module. A newly installed or newly fixed
 *     extension does not take effect until the Shell restarts, which under Wayland
 *     means logging out. This window shows the cluster immediately.
 *  2. The Shell extension only works on GNOME. This works on any desktop that can run
 *     a GTK4 window.
 *  3. It is the fastest way to iterate on the drawing, because it is live rather than
 *     a PNG.
 *
 * It lives beside the extension because it shares `gauges.js` verbatim — one copy of
 * the rendering, three consumers (Shell widget, this window, the PNG harness).
 *
 * Undecorated and sized to its contents, with the whole surface acting as a drag
 * handle, so it behaves like a desktop gadget rather than an application window.
 * Right-click the panel for "Always on Top" if your compositor offers it.
 */

import Gtk from 'gi://Gtk?version=4.0';
import Gdk from 'gi://Gdk?version=4.0';
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';

import {drawCluster, CLUSTER_W, CLUSTER_H} from './gauges.js';

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

/* -------------------------------------------------------------- arguments */

function parseArgs(argv) {
    const o = {scale: 1.0, opacity: 0.92, decorated: false};
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        if (a === '--scale') o.scale = parseFloat(argv[++i]) || 1.0;
        else if (a === '--opacity') o.opacity = parseFloat(argv[++i]) || 0.92;
        else if (a === '--decorated') o.decorated = true;
        else if (a === '--help' || a === '-h') o.help = true;
    }
    o.scale = Math.max(0.55, Math.min(2.5, o.scale));
    o.opacity = Math.max(0.2, Math.min(1.0, o.opacity));
    return o;
}

const opts = parseArgs(ARGV ?? []);
if (opts.help) {
    print(`jamsys-cluster — the JamSys instrument cluster in its own window

USAGE:
  jamsys-cluster [--scale N] [--opacity N] [--decorated]

  --scale N      0.55 to 2.5, default 1.0 (${CLUSTER_W} x ${CLUSTER_H} px)
  --opacity N    0.2 to 1.0, default 0.92
  --decorated    keep the title bar; by default the window is a bare panel you
                 drag by its face

  Click the cluster to open the full JamSys window.
  Escape or Ctrl+Q closes it.`);
    imports.system.exit(0);
}

/* ------------------------------------------------------------------- app */

const app = new Gtk.Application({
    application_id: 'org.jamsys.Cluster',
    flags: Gio.ApplicationFlags.NON_UNIQUE,
});

let state = null;
let area = null;
let proxy = null;
let signalId = 0;

app.connect('activate', () => {
    const win = new Gtk.ApplicationWindow({
        application: app,
        title: 'JamSys',
        decorated: opts.decorated,
        resizable: false,
    });
    win.set_default_size(Math.round(CLUSTER_W * opts.scale),
                         Math.round(CLUSTER_H * opts.scale));

    // The cluster paints its own rounded housing, so everything behind it must be
    // transparent or the corners sit on a grey square.
    const css = new Gtk.CssProvider();
    css.load_from_data(
        'window, window.background { background: transparent; box-shadow: none; }', -1);
    Gtk.StyleContext.add_provider_for_display(
        Gdk.Display.get_default(), css, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION);

    area = new Gtk.DrawingArea({
        content_width: Math.round(CLUSTER_W * opts.scale),
        content_height: Math.round(CLUSTER_H * opts.scale),
    });
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
            drawCluster(cr, w, h, state, {opacity: opts.opacity});
        } catch (e) {
            printerr(`jamsys-cluster: draw failed: ${e.message}`);
        }
    });

    // The whole face is the drag handle, since there is no title bar to grab.
    const handle = new Gtk.WindowHandle({child: area});
    win.set_child(handle);

    // Click opens the full window, on the page for whatever is wrong.
    const click = new Gtk.GestureClick({button: 1});
    click.connect('released', () => {
        const page = state?.alert_subsystem;
        const argv = page ? ['jamsys', '--page', page] : ['jamsys'];
        try {
            Gio.Subprocess.new(argv, Gio.SubprocessFlags.NONE);
        } catch (e) {
            printerr(`jamsys-cluster: could not launch jamsys: ${e.message}`);
        }
    });
    area.add_controller(click);

    const keys = new Gtk.EventControllerKey();
    keys.connect('key-pressed', (_c, keyval, _code, mods) => {
        if (keyval === Gdk.KEY_Escape ||
            (keyval === Gdk.KEY_q && (mods & Gdk.ModifierType.CONTROL_MASK))) {
            win.close();
            return true;
        }
        return false;
    });
    win.add_controller(keys);

    win.present();
    connectDaemon();
});

function connectDaemon() {
    try {
        proxy = new Proxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH);
    } catch (e) {
        printerr(`jamsys-cluster: cannot reach ${BUS_NAME}: ${e.message}`);
        printerr('  is the daemon running?  systemctl --user status jamsysd');
        // Keep trying: the daemon may simply not be up yet.
        GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 3, () => {
            connectDaemon();
            return false;
        });
        return;
    }
    signalId = proxy.connectSignal('StateChanged', (_p, _s, [json]) => apply(json));
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
