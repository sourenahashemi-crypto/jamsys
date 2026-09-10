/* JamSys — GNOME Shell desktop instrument cluster.
 *
 * This extension contains **no monitoring logic whatsoever**. It is a renderer.
 * Everything it shows arrives as a single JSON string on `StateChanged` from the
 * JamSys daemon, which does all the sampling, anomaly detection and — importantly —
 * all the change detection. The daemon only emits when a displayed value has actually
 * changed enough for a human to notice, so this repaints rarely and never polls.
 *
 * That division matters: this runs inside the compositor process. Work done here
 * janks the whole desktop.
 *
 * Three presentations:
 *   cluster  a floating instrument binnacle in a screen corner (the default)
 *   compact  one line of text in the top panel
 *   minimal  the same readings stacked
 *
 * The cluster is a desktop gadget, so it is always a `layoutManager` chrome actor —
 * the same mechanism OSD popups use. Under Wayland there is no such thing as an
 * always-on-top application window, and faking one is not attempted.
 */

import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import GObject from 'gi://GObject';
import St from 'gi://St';
import Clutter from 'gi://Clutter';

import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';

import {labelFor, normalLine, styleFor} from './format.js';
import {drawCluster, drawClusterBare, drawUnavailable,
        CLUSTER_W, CLUSTER_H, BARE_W, BARE_H} from './gauges.js';

/* Must match the range in the gschema, or set_double() is silently clamped by
 * GSettings and the widget stops responding at the edges with no explanation. */
const SCALE_MIN = 0.55;
const SCALE_MAX = 3.0;

const BUS_NAME = 'org.jamsys.Daemon';
const OBJECT_PATH = '/org/jamsys/Daemon';

const JamSysIface = `
<node>
  <interface name="org.jamsys.Daemon">
    <method name="GetState"><arg type="s" direction="out" name="state"/></method>
    <method name="GetVersion"><arg type="s" direction="out" name="version"/></method>
    <method name="AckTopAlert"><arg type="b" direction="out" name="ok"/></method>
    <signal name="StateChanged"><arg type="s" name="state"/></signal>
  </interface>
</node>`;

const JamSysProxy = Gio.DBusProxy.makeProxyWrapper(JamSysIface);

/* ------------------------------------------------------------- the cluster */

const Cluster = GObject.registerClass(
class JamSysCluster extends St.Widget {
    _init(ext) {
        super._init({
            reactive: true,
            track_hover: true,
            can_focus: true,
            layout_manager: new Clutter.BinLayout(),
        });
        this._ext = ext;
        this._settings = ext.getSettings();
        this._state = null;

        this._area = new St.DrawingArea({x_expand: true, y_expand: true});
        this._area.connect('repaint', a => this._repaint(a));
        this.add_child(this._area);

        // Left-click used to open the main window. It should not: the gadget is
        // something you glance at and drag around, and every stray click on it
        // launching an application is the opposite of calm. Opening is now a
        // deliberate act -- the right-click menu, or a double click.
        this.connect('button-press-event', (_a, event) => {
            if (event.get_button() === 3) {
                this._showMenu();
                return Clutter.EVENT_STOP;
            }
            if (event.get_button() === 1 && event.get_click_count() === 2) {
                this._ext.openApp(this._state?.alert_subsystem);
                return Clutter.EVENT_STOP;
            }
            return Clutter.EVENT_PROPAGATE;   // let the drag action have it
        });

        // Drag to place it anywhere, not just the five preset corners. The drag
        // action owns the pointer grab and only starts past Clutter's threshold,
        // so a plain click is still a click.
        this._drag = new Clutter.DragAction();
        this._drag.connect('drag-end', () => this._onDragEnd());
        this.add_action(this._drag);
        // Hover lifts the housing slightly, so it reads as a clickable object.
        this.connect('notify::hover', () => this._area.queue_repaint());

        // Scroll resizes, exactly as it does on the standalone window. Without
        // this the corner widget could only be resized from the preferences
        // dialog, which is not where anyone looks when a gadget is the wrong size.
        this.connect('scroll-event', (_a, event) => this._onScroll(event));

        this._resize();
    }

    /** Remember where it was dropped, and stop the corner logic overriding it. */
    _onDragEnd() {
        const [x, y] = this.get_position();
        this._settings.set_int('custom-x', Math.round(x));
        this._settings.set_int('custom-y', Math.round(y));
        // Writing 'position' last: it is a rebuild key, so the widget is placed
        // from the coordinates that are already stored.
        this._settings.set_string('position', 'custom');
    }

    _showMenu() {
        if (!this._menu) {
            this._menu = new PopupMenu.PopupMenu(this, 0.5, St.Side.TOP);
            this._menu.actor.add_style_class_name('jamsys-cluster-menu');

            const open = new PopupMenu.PopupMenuItem('Open JamSys');
            open.connect('activate', () => this._ext.openApp(this._state?.alert_subsystem));
            this._menu.addMenuItem(open);

            this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

            const bigger = new PopupMenu.PopupMenuItem('Bigger');
            bigger.connect('activate', () => this._nudgeScale(1.15));
            this._menu.addMenuItem(bigger);

            const smaller = new PopupMenu.PopupMenuItem('Smaller');
            smaller.connect('activate', () => this._nudgeScale(1 / 1.15));
            this._menu.addMenuItem(smaller);

            const reset = new PopupMenu.PopupMenuItem('Reset size and position');
            reset.connect('activate', () => {
                this._settings.set_double('scale', 1.0);
                this._settings.set_int('custom-x', -1);
                this._settings.set_int('custom-y', -1);
                this._settings.set_string('position', 'top-right');
            });
            this._menu.addMenuItem(reset);

            this._menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

            const hide = new PopupMenu.PopupMenuItem('Hide the cluster');
            hide.connect('activate', () => {
                // Not a 'hidden' flag with no way back: switch to the panel line,
                // which keeps one small thing on screen whose own menu can bring
                // the cluster back. A gadget you cannot un-hide is a lost gadget.
                this._settings.set_string('mode', 'minimal');
            });
            this._menu.addMenuItem(hide);

            const prefs = new PopupMenu.PopupMenuItem('Preferences');
            prefs.connect('activate', () => this._ext.openPreferences());
            this._menu.addMenuItem(prefs);

            Main.uiGroup.add_child(this._menu.actor);
            this._menu.actor.hide();
            this._menuManager = new PopupMenu.PopupMenuManager(this);
            this._menuManager.addMenu(this._menu);
        }
        this._menu.toggle();
    }

    _nudgeScale(factor) {
        const cur = this._settings.get_double('scale');
        const next = Math.max(SCALE_MIN, Math.min(SCALE_MAX, cur * factor));
        this._settings.set_double('scale', Math.round(next * 1000) / 1000);
    }

    /** Scroll up grows, scroll down shrinks; the setting is the single source. */
    _onScroll(event) {
        const dir = event.get_scroll_direction();
        let up;
        if (dir === Clutter.ScrollDirection.UP) {
            up = true;
        } else if (dir === Clutter.ScrollDirection.DOWN) {
            up = false;
        } else if (dir === Clutter.ScrollDirection.SMOOTH) {
            // Wayland delivers smooth scroll, so handling only UP/DOWN would mean
            // handling nothing at all on the compositor this targets.
            const [, dy] = event.get_scroll_delta();
            if (!Number.isFinite(dy) || Math.abs(dy) < 0.01)
                return Clutter.EVENT_PROPAGATE;
            up = dy < 0;
        } else {
            return Clutter.EVENT_PROPAGATE;
        }

        const cur = this._settings.get_double('scale');
        const next = Math.max(SCALE_MIN, Math.min(SCALE_MAX, cur * (up ? 1.08 : 1 / 1.08)));
        // Writing the setting is what resizes: 'scale' is already wired to
        // _restyle(), which resizes and repositions. Rounded so that repeated
        // scrolling cannot accumulate floating-point drift in a stored value.
        if (Math.abs(next - cur) > 1e-4)
            this._settings.set_double('scale', Math.round(next * 1000) / 1000);
        return Clutter.EVENT_STOP;
    }

    /** 'cutout' by default: the same face the standalone window draws. */
    _cutout() {
        return this._settings.get_string('style') !== 'housing';
    }

    _resize() {
        const k = this._settings.get_double('scale');
        // The two faces have different natural sizes, so the actor has to follow
        // whichever is in force or the drawing is letterboxed inside a stale box.
        const cut = this._cutout();
        this._w = Math.round((cut ? BARE_W : CLUSTER_W) * k);
        this._h = Math.round((cut ? BARE_H : CLUSTER_H) * k);
        this.set_size(this._w, this._h);
        this._area.set_size(this._w, this._h);
        this._area.queue_repaint();
    }

    setState(state) {
        this._state = state;
        this._area.queue_repaint();
    }

    _repaint(area) {
        const [w, h] = area.get_surface_size();
        const cr = area.get_context();
        try {
            if (!this._state) {
                drawUnavailable(cr, w, h);
                return;
            }
            let opacity = this._settings.get_double('opacity');
            if (this.hover) opacity = Math.min(1, opacity + 0.06);
            if (this._cutout()) drawClusterBare(cr, w, h, this._state, {opacity});
            else drawCluster(cr, w, h, this._state, {opacity});
        } catch (e) {
            // A drawing bug must never take down the compositor.
            logError(e, 'JamSys: cluster repaint failed');
        } finally {
            cr.$dispose();
        }
    }
});

/* --------------------------------------------------------- the panel text */

const Indicator = GObject.registerClass(
class JamSysIndicator extends PanelMenu.Button {
    _init(ext) {
        super._init(0.0, 'JamSys', false);
        this._ext = ext;
        this._settings = ext.getSettings();
        this._state = null;

        this._label = new St.Label({
            text: 'JamSys …',
            y_align: Clutter.ActorAlign.CENTER,
            style_class: 'jamsys-label jamsys-unknown',
        });
        this.add_child(this._label);
        this._buildMenu();
    }

    _buildMenu() {
        this._titleItem = new PopupMenu.PopupMenuItem('', {reactive: false});
        this._titleItem.label.add_style_class_name('jamsys-menu-title');
        this.menu.addMenuItem(this._titleItem);

        this._detailItem = new PopupMenu.PopupMenuItem('', {reactive: false});
        this._detailItem.label.add_style_class_name('jamsys-menu-detail');
        this._detailItem.label.clutter_text.line_wrap = true;
        this.menu.addMenuItem(this._detailItem);

        this._expectedItem = new PopupMenu.PopupMenuItem('', {reactive: false});
        this._expectedItem.label.add_style_class_name('jamsys-menu-expected');
        this._expectedItem.label.clutter_text.line_wrap = true;
        this.menu.addMenuItem(this._expectedItem);

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());

        const open = new PopupMenu.PopupMenuItem('Open JamSys');
        open.connect('activate', () => this._ext.openApp(this._state?.alert_subsystem));
        this.menu.addMenuItem(open);

        this._ackItem = new PopupMenu.PopupMenuItem('Acknowledge this alert');
        this._ackItem.connect('activate', () => this._ext.ackTopAlert());
        this.menu.addMenuItem(this._ackItem);

        // The way back from "Hide the cluster". Without this the panel line would
        // be a one-way door and the corner gadget could only be restored from the
        // preferences dialog.
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const showCluster = new PopupMenu.PopupMenuItem('Show the corner cluster');
        showCluster.connect('activate', () =>
            this._settings.set_string('mode', 'cluster'));
        this.menu.addMenuItem(showCluster);
    }

    setState(state) {
        this._state = state;
        this._render();
    }

    setUnavailable(reason) {
        this._state = null;
        this._label.text = 'JamSys — offline';
        this._setClass('jamsys-unknown');
        this._titleItem.label.text = 'The monitoring service is not running';
        this._detailItem.label.text = reason || '';
        this._expectedItem.label.text = 'systemctl --user start jamsysd';
        this._detailItem.visible = true;
        this._expectedItem.visible = true;
        this._ackItem.visible = false;
    }

    _setClass(cls) {
        for (const c of ['jamsys-healthy', 'jamsys-attention',
                         'jamsys-critical', 'jamsys-unknown'])
            this._label.remove_style_class_name(c);
        this._label.add_style_class_name(cls);
        this._label.remove_style_class_name('jamsys-minimal');
        if (this._settings.get_string('mode') === 'minimal')
            this._label.add_style_class_name('jamsys-minimal');
    }

    _showFlags() {
        const f = {};
        for (const k of ['show-cpu', 'show-temp', 'show-ram', 'show-gpu', 'show-power', 'show-net'])
            f[k] = this._settings.get_boolean(k);
        return f;
    }

    _render() {
        const s = this._state;
        if (!s)
            return;
        const minimal = this._settings.get_string('mode') === 'minimal';
        const abnormal = s.health === 'attention' || s.health === 'critical';

        this._label.text = labelFor(s, this._showFlags(), minimal);
        this._setClass(styleFor(s.health));

        this._titleItem.label.text = s.alert_title || 'Everything is normal';
        this._detailItem.label.text = s.alert_detail || normalLine(s, this._showFlags(), false);
        this._detailItem.visible = !!(s.alert_detail || !abnormal);
        this._expectedItem.label.text = s.alert_expected ? `Expected: ${s.alert_expected}` : '';
        this._expectedItem.visible = !!s.alert_expected;
        this._ackItem.visible = abnormal;
    }
});

/* ----------------------------------------------------------------- extension */

export default class JamSysExtension extends Extension {
    enable() {
        this._settings = this.getSettings();
        this._widget = null;      // Cluster or Indicator, whichever the mode wants
        this._floating = null;    // the chrome container, when floating
        this._proxy = null;
        this._signalId = 0;
        this._watchId = 0;
        this._lastState = null;
        this._reconnectId = 0;
        this._connectionGeneration = (this._connectionGeneration ?? 0) + 1;

        this._build();

        // Anything that changes the shape of the widget rebuilds it; anything that
        // only changes its appearance just repaints.
        this._rebuildIds = ['mode', 'position', 'style'].map(k =>
            this._settings.connect(`changed::${k}`, () => this._rebuild()));
        this._redrawIds = ['scale', 'opacity', 'margin', 'custom-x', 'custom-y',
                           'show-cpu', 'show-temp',
                           'show-ram', 'show-gpu', 'show-power', 'show-net',
                           'dim-when-healthy'].map(k =>
            this._settings.connect(`changed::${k}`, () => this._restyle()));

        this._watchId = Gio.bus_watch_name(
            Gio.BusType.SESSION, BUS_NAME, Gio.BusNameWatcherFlags.NONE,
            () => this._connect(),
            () => this._onVanished());
    }

    disable() {
        if (this._watchId) {
            Gio.bus_unwatch_name(this._watchId);
            this._watchId = 0;
        }
        if (this._reconnectId) {
            GLib.Source.remove(this._reconnectId);
            this._reconnectId = 0;
        }
        this._disconnect();
        for (const id of [...(this._rebuildIds ?? []), ...(this._redrawIds ?? [])])
            this._settings.disconnect(id);
        this._rebuildIds = this._redrawIds = null;
        this._teardown();
        this._settings = null;
        this._lastState = null;
    }

    /* -- widget lifecycle ------------------------------------------- */

    _isCluster() {
        return this._settings.get_string('mode') === 'cluster';
    }

    _build() {
        const pos = this._settings.get_string('position');
        const cluster = this._isCluster();
        // A 420px instrument binnacle has no business in the top panel, so cluster
        // mode always floats; the position setting only picks which corner.
        const floating = cluster || pos.startsWith('bottom');

        this._widget = cluster ? new Cluster(this) : new Indicator(this);

        if (floating) {
            this._floating = new St.Bin({
                style_class: cluster ? 'jamsys-cluster' : 'jamsys-floating',
                reactive: true,
                child: this._widget,
            });
            // addChrome takes only affectsStruts and trackFullscreen; it manages the
            // input region itself, and GNOME 50 rejects the extra key outright rather
            // than ignoring it. affectsStruts:false keeps the cluster from reserving
            // screen space, trackFullscreen:true hides it under fullscreen windows.
            Main.layoutManager.addChrome(this._floating, {
                affectsStruts: false,
                trackFullscreen: true,
            });
            this._monitorId = Main.layoutManager.connect('monitors-changed',
                                                         () => this._reposition());
            this._allocId = this._floating.connect('notify::allocation',
                                                   () => this._reposition());
            this._reposition();
        } else {
            const box = pos === 'top-left' ? 'left' : pos === 'top-center' ? 'center' : 'right';
            Main.panel.addToStatusArea(this.uuid, this._widget, box === 'left' ? 10 : 0, box);
        }

        if (this._lastState)
            this._widget.setState(this._lastState);
        else
            this._widget.setUnavailable?.('Waiting for jamsysd.');
    }

    _teardown() {
        // The popup lives in uiGroup, outside the widget's actor tree. Rebuilds
        // must release it too, not only the extension's final disable().
        if (this._widget?._menu) {
            this._widget._menu.destroy();
            this._widget._menu = null;
            this._widget._menuManager = null;
        }
        if (this._floating) {
            if (this._allocId) {
                this._floating.disconnect(this._allocId);
                this._allocId = 0;
            }
            if (this._monitorId) {
                Main.layoutManager.disconnect(this._monitorId);
                this._monitorId = 0;
            }
            Main.layoutManager.removeChrome(this._floating);
            this._floating.destroy();   // destroys the child widget too
            this._floating = null;
            this._widget = null;
        } else if (this._widget) {
            this._widget.destroy();
            this._widget = null;
        }
    }

    _rebuild() {
        this._teardown();
        this._build();
    }

    _restyle() {
        if (this._widget instanceof Cluster) {
            this._widget._resize();
            this._reposition();
        } else if (this._widget && this._lastState) {
            this._widget.setState(this._lastState);
        }
    }

    _reposition() {
        if (!this._floating)
            return;
        const mon = Main.layoutManager.primaryMonitor;
        if (!mon)
            return;
        const m = this._settings.get_int('margin');
        const [w, h] = this._floating.get_size();
        const pos = this._settings.get_string('position');

        // A dragged widget keeps where it was dropped. Clamped to the monitor so a
        // drop near an edge -- or a later change of resolution, or growing the
        // widget past the edge with the scroll wheel -- can never strand it
        // off-screen where it cannot be dragged back.
        if (pos === 'custom') {
            const cx = this._settings.get_int('custom-x');
            const cy = this._settings.get_int('custom-y');
            if (cx >= 0 || cy >= 0) {
                const x = Math.max(mon.x, Math.min(cx, mon.x + mon.width - w));
                const y = Math.max(mon.y + Main.panel.height,
                                   Math.min(cy, mon.y + mon.height - h));
                this._floating.set_position(x, y);
                return;
            }
        }

        const top = pos.startsWith('top');
        // The panel is only in the way for top placements.
        const topInset = top ? Main.panel.height + m : m;
        let x;
        if (pos.endsWith('center')) x = mon.x + Math.round((mon.width - w) / 2);
        else if (pos.endsWith('left')) x = mon.x + m;
        else x = mon.x + mon.width - w - m;
        const y = top ? mon.y + topInset : mon.y + mon.height - h - m;
        this._floating.set_position(x, y);
    }

    /* -- daemon ------------------------------------------------------ */

    _connect() {
        this._disconnect();
        const generation = this._connectionGeneration;
        try {
            this._proxy = new JamSysProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH);
        } catch (e) {
            // _connect() is only ever the name-appeared callback, so the name IS
            // owned and the daemon IS running. Reporting "jamsysd is not running"
            // here sent the user to fix something that was not broken.
            logError(e, 'JamSys: could not create the D-Bus proxy');
            this._proxy = null;
            this._lastState = null;
            this._widget?.setState(null);
            this._widget?.setUnavailable?.(
                'The monitoring service is running but could not be reached.');
            // name_appeared fires once per ownership change, so without a retry a
            // single transient failure would strand the widget until the daemon
            // itself restarted.
            this._scheduleReconnect();
            return;
        }
        let pushed = false;
        this._signalId = this._proxy.connectSignal('StateChanged', (_p, _s, [json]) => {
            if (generation !== this._connectionGeneration) return;
            pushed = true;
            this._apply(json);
        });
        this._proxy.GetStateRemote(([json], err) => {
            // Replies can arrive after disable/reconnect, or after a newer push.
            if (generation !== this._connectionGeneration || pushed) return;
            if (err) {
                this._lastState = null;
                this._widget?.setState(null);
                this._widget?.setUnavailable?.(String(err.message ?? err));
                return;
            }
            this._apply(json);
        });
    }

    _onVanished() {
        this._disconnect();
        this._lastState = null;
        this._widget?.setUnavailable?.('jamsysd is not running.');
        if (this._widget instanceof Cluster)
            this._widget.setState(null);
    }

    /** One bounded retry, for a failure that the name watch will not repeat. */
    _scheduleReconnect() {
        if (this._reconnectId) return;
        this._reconnectId = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 10, () => {
            this._reconnectId = 0;
            if (!this._proxy && this._settings) this._connect();
            return false;
        });
    }

    _disconnect() {
        this._connectionGeneration++;
        if (this._signalId && this._proxy)
            this._proxy.disconnectSignal(this._signalId);
        this._signalId = 0;
        this._proxy = null;
    }

    _apply(json) {
        let state;
        try {
            state = JSON.parse(json);
        } catch (e) {
            logError(e, 'JamSys: bad state payload');
            return;
        }
        this._lastState = state;
        this._widget?.setState(state);
    }

    /* -- actions ------------------------------------------------------ */

    openApp(page) {
        const argv = page ? ['jamsys', '--page', page] : ['jamsys'];
        try {
            const p = new Gio.Subprocess({argv, flags: Gio.SubprocessFlags.NONE});
            p.init(null);
        } catch (e) {
            logError(e, 'JamSys: could not launch the application');
            Main.notify('JamSys', 'Could not launch the JamSys window.');
        }
    }

    ackTopAlert() {
        this._proxy?.AckTopAlertRemote(() => {});
    }
}
