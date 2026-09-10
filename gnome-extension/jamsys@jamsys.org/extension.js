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
import {drawCluster, drawClusterBare,
        CLUSTER_W, CLUSTER_H, BARE_W, BARE_H} from './gauges.js';

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

        this.connect('button-press-event', () => {
            this._ext.openApp(this._state?.alert_subsystem);
            return Clutter.EVENT_STOP;
        });
        // Hover lifts the housing slightly, so it reads as a clickable object.
        this.connect('notify::hover', () => this._area.queue_repaint());

        this._resize();
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
                // Say nothing rather than draw meaningless zeroed instruments.
                cr.setSourceRGBA(0.03, 0.04, 0.05, 0.75);
                cr.paint();
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

        this._build();

        // Anything that changes the shape of the widget rebuilds it; anything that
        // only changes its appearance just repaints.
        this._rebuildIds = ['mode', 'position', 'style'].map(k =>
            this._settings.connect(`changed::${k}`, () => this._rebuild()));
        this._redrawIds = ['scale', 'opacity', 'margin', 'show-cpu', 'show-temp',
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
        if (this._signalId && this._proxy) {
            this._proxy.disconnectSignal(this._signalId);
            this._signalId = 0;
        }
        this._proxy = null;
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
    }

    _teardown() {
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
        try {
            this._proxy = new JamSysProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH);
        } catch (e) {
            logError(e, 'JamSys: could not create the D-Bus proxy');
            this._widget?.setUnavailable?.('Could not reach the monitoring service.');
            return;
        }
        this._signalId = this._proxy.connectSignal('StateChanged', (_p, _s, [json]) => {
            this._apply(json);
        });
        this._proxy.GetStateRemote(([json], err) => {
            if (err) {
                this._widget?.setUnavailable?.(String(err.message ?? err));
                return;
            }
            this._apply(json);
        });
    }

    _onVanished() {
        this._proxy = null;
        this._lastState = null;
        this._widget?.setUnavailable?.('jamsysd is not running.');
        if (this._widget instanceof Cluster)
            this._widget.setState(null);
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
