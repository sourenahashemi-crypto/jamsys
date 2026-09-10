/* JamSys — GNOME Shell corner readout.
 *
 * This extension contains **no monitoring logic whatsoever**. It is a renderer.
 * Everything it shows arrives as a single JSON string on `StateChanged` from the
 * JamSys daemon, which does all the sampling, anomaly detection and — importantly —
 * all the change detection. The daemon only emits when a displayed value has actually
 * changed enough for a human to notice, so this code repaints rarely and never polls.
 *
 * That division matters: this runs inside the compositor process. Work done here
 * janks the whole desktop.
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

// Pure formatting lives in its own module so it can be unit-tested outside
// gnome-shell, where none of the above imports resolve.
import {labelFor, normalLine, shortAlert, styleFor, pct, deg, watt} from './format.js';

const BUS_NAME = 'org.jamsys.Daemon';
const OBJECT_PATH = '/org/jamsys/Daemon';
const IFACE = 'org.jamsys.Daemon';

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

        // Re-render, not re-fetch, when presentation preferences change.
        this._settingsIds = [
            'mode', 'show-cpu', 'show-temp', 'show-ram', 'show-gpu',
            'show-power', 'show-net', 'dim-when-healthy',
        ].map(k => this._settings.connect(`changed::${k}`, () => this._render()));
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

        this._openItem = new PopupMenu.PopupMenuItem('Open JamSys');
        this._openItem.connect('activate', () => this._openApp());
        this.menu.addMenuItem(this._openItem);

        this._ackItem = new PopupMenu.PopupMenuItem('Acknowledge this alert');
        this._ackItem.connect('activate', () => this._ext.ackTopAlert());
        this.menu.addMenuItem(this._ackItem);
    }

    /** Launch the detailed application, jumping straight to the relevant page. */
    _openApp() {
        const page = this._state?.alert_subsystem || '';
        const argv = page ? ['jamsys', '--page', page] : ['jamsys'];
        try {
            const p = new Gio.Subprocess({argv, flags: Gio.SubprocessFlags.NONE});
            p.init(null);
        } catch (e) {
            // A missing binary must not throw inside the compositor.
            logError(e, 'JamSys: could not launch the application');
            Main.notify('JamSys', 'Could not launch the JamSys window.');
        }
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

        // When something is wrong the readout shows only that: the spec asks for the
        // important abnormal metric prominently, not buried in a row of healthy ones.
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

export default class JamSysExtension extends Extension {
    enable() {
        this._settings = this.getSettings();
        this._indicator = new Indicator(this);
        this._floating = null;
        this._proxy = null;
        this._retryId = 0;
        this._watchId = 0;

        this._place();
        this._positionId = this._settings.connect('changed::position', () => {
            this._unplace();
            this._place();
        });

        // Follow the daemon coming and going rather than polling for it.
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
        if (this._retryId) {
            GLib.Source.remove(this._retryId);
            this._retryId = 0;
        }
        if (this._signalId && this._proxy) {
            this._proxy.disconnectSignal(this._signalId);
            this._signalId = 0;
        }
        this._proxy = null;
        if (this._positionId) {
            this._settings.disconnect(this._positionId);
            this._positionId = 0;
        }
        this._unplace();
        this._indicator?.destroy();
        this._indicator = null;
        this._settings = null;
    }

    /* -- placement ---------------------------------------------------- */

    _place() {
        const pos = this._settings.get_string('position');
        if (pos === 'bottom-left' || pos === 'bottom-right') {
            this._placeFloating(pos);
        } else {
            const box = pos === 'top-left' ? 'left' : pos === 'top-center' ? 'center' : 'right';
            // Index 0 in the left box would sit before Activities; use a late index
            // there and an early one on the right so the readout lands at the corner.
            Main.panel.addToStatusArea(this.uuid, this._indicator,
                                       box === 'left' ? 10 : 0, box);
        }
    }

    /* GNOME Shell's panel is top-only. Bottom placement therefore uses a chrome
     * actor — the same mechanism as OSD popups — rather than a fake always-on-top
     * window, which cannot work under Wayland. It is supported but genuinely less
     * robust than the panel: it can overlap a dock or a maximised window's shadow,
     * and it is hidden under fullscreen. See README for the caveats. */
    _placeFloating(pos) {
        this._floating = new St.Bin({
            style_class: 'jamsys-floating',
            reactive: true,
            track_hover: true,
        });
        if (this._indicator.get_parent())
            this._indicator.get_parent().remove_child(this._indicator);
        this._floating.set_child(this._indicator);
        Main.layoutManager.addChrome(this._floating, {
            trackFullscreen: true,
            affectsStruts: false,
            affectsInputRegion: true,
        });
        const reposition = () => {
            const mon = Main.layoutManager.primaryMonitor;
            if (!mon || !this._floating)
                return;
            const [w, h] = this._floating.get_size();
            const x = pos === 'bottom-left' ? mon.x + 8 : mon.x + mon.width - w - 8;
            this._floating.set_position(x, mon.y + mon.height - h - 8);
        };
        this._allocId = this._floating.connect('notify::allocation', reposition);
        this._monitorId = Main.layoutManager.connect('monitors-changed', reposition);
        reposition();
    }

    _unplace() {
        if (this._floating) {
            if (this._allocId) {
                this._floating.disconnect(this._allocId);
                this._allocId = 0;
            }
            if (this._monitorId) {
                Main.layoutManager.disconnect(this._monitorId);
                this._monitorId = 0;
            }
            if (this._indicator?.get_parent() === this._floating)
                this._floating.remove_child(this._indicator);
            Main.layoutManager.removeChrome(this._floating);
            this._floating.destroy();
            this._floating = null;
        } else if (this._indicator?.container?.get_parent()) {
            // addToStatusArea inserts the indicator's container; removing the
            // indicator from the status area is handled by destroy().
        }
    }

    /* -- daemon connection -------------------------------------------- */

    _connect() {
        try {
            this._proxy = new JamSysProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH);
        } catch (e) {
            logError(e, 'JamSys: could not create the D-Bus proxy');
            this._indicator?.setUnavailable('Could not reach the monitoring service.');
            return;
        }
        this._signalId = this._proxy.connectSignal('StateChanged', (_p, _s, [json]) => {
            this._apply(json);
        });
        // One read for the initial paint; everything after that is pushed.
        this._proxy.GetStateRemote(([json], err) => {
            if (err) {
                this._indicator?.setUnavailable(String(err.message ?? err));
                return;
            }
            this._apply(json);
        });
    }

    _onVanished() {
        this._proxy = null;
        this._indicator?.setUnavailable('jamsysd is not running.');
    }

    _apply(json) {
        let state;
        try {
            state = JSON.parse(json);
        } catch (e) {
            // Malformed input must never take down the Shell.
            logError(e, 'JamSys: bad state payload');
            return;
        }
        this._indicator?.setState(state);
    }

    ackTopAlert() {
        this._proxy?.AckTopAlertRemote(() => {});
    }
}
