/* Preferences for the JamSys desktop cluster. Presentation only — nothing here
 * affects what is monitored, which is entirely the daemon's business. */

import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

const POSITIONS = ['top-left', 'top-center', 'top-right', 'bottom-left', 'bottom-right'];
const POSITION_LABELS = ['Top left', 'Top centre', 'Top right', 'Bottom left', 'Bottom right'];
const MODES = ['cluster', 'compact', 'minimal'];
const STYLES = ['cutout', 'housing'];
const STYLE_LABELS = ['Cut-out dials (no panel)', 'Panel housing'];
const MODE_LABELS = ['Instrument cluster', 'Compact line (panel)', 'Stacked lines (panel)'];

export default class JamSysPrefs extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        const settings = this.getSettings();
        const page = new Adw.PreferencesPage({title: 'Display', icon_name: 'video-display-symbolic'});
        window.add(page);

        // -- presentation ------------------------------------------------
        const look = new Adw.PreferencesGroup({
            title: 'Presentation',
            description: 'The instrument cluster is a desktop gadget and floats in a '
                       + 'corner. The two line modes live in the top panel instead.',
        });
        page.add(look);

        const modeRow = new Adw.ComboRow({
            title: 'Style',
            model: Gtk.StringList.new(MODE_LABELS),
        });
        modeRow.set_selected(Math.max(0, MODES.indexOf(settings.get_string('mode'))));
        modeRow.connect('notify::selected', r =>
            settings.set_string('mode', MODES[r.get_selected()]));
        look.add(modeRow);

        const styleRow = new Adw.ComboRow({
            title: 'Face',
            subtitle: 'Cut-out shows four dials directly on the wallpaper, with '
                    + 'network throughput; the panel is the older boxed layout',
            model: Gtk.StringList.new(STYLE_LABELS),
        });
        styleRow.set_selected(Math.max(0, STYLES.indexOf(settings.get_string('style'))));
        styleRow.connect('notify::selected', r =>
            settings.set_string('style', STYLES[r.get_selected()]));
        look.add(styleRow);

        const posRow = new Adw.ComboRow({
            title: 'Corner',
            subtitle: 'Where the cluster sits, or which end of the panel the line uses',
            model: Gtk.StringList.new(POSITION_LABELS),
        });
        posRow.set_selected(Math.max(0, POSITIONS.indexOf(settings.get_string('position'))));
        posRow.connect('notify::selected', r =>
            settings.set_string('position', POSITIONS[r.get_selected()]));
        look.add(posRow);

        // -- cluster --------------------------------------------------------
        const cluster = new Adw.PreferencesGroup({
            title: 'Instrument cluster',
            description: 'GNOME Shell has no bottom panel, so the cluster is drawn as a '
                       + 'floating desktop element. It stays out of the way of maximised '
                       + 'windows and is hidden by fullscreen ones.',
        });
        page.add(cluster);

        const scale = new Adw.SpinRow({
            title: 'Size',
            subtitle: '1.0 is 420 × 192 pixels',
            adjustment: new Gtk.Adjustment({lower: 0.55, upper: 1.8, step_increment: 0.05}),
            digits: 2,
        });
        settings.bind('scale', scale, 'value', Gio.SettingsBindFlags.DEFAULT);
        cluster.add(scale);

        const opacity = new Adw.SpinRow({
            title: 'Housing opacity',
            subtitle: 'The instruments themselves stay fully opaque',
            adjustment: new Gtk.Adjustment({lower: 0.35, upper: 1.0, step_increment: 0.02}),
            digits: 2,
        });
        settings.bind('opacity', opacity, 'value', Gio.SettingsBindFlags.DEFAULT);
        cluster.add(opacity);

        const margin = new Adw.SpinRow({
            title: 'Distance from the screen edge',
            adjustment: new Gtk.Adjustment({lower: 0, upper: 120, step_increment: 2}),
        });
        settings.bind('margin', margin, 'value', Gio.SettingsBindFlags.DEFAULT);
        cluster.add(margin);

        // -- line modes -------------------------------------------------------
        const shown = new Adw.PreferencesGroup({
            title: 'Panel line: what to show while healthy',
            description: 'Applies to the two line styles. When something is abnormal the '
                       + 'line replaces all of this with the one thing that is wrong.',
        });
        page.add(shown);

        for (const [key, title] of [
            ['show-cpu', 'CPU usage'],
            ['show-temp', 'CPU temperature'],
            ['show-ram', 'Memory'],
            ['show-gpu', 'GPU'],
            ['show-power', 'Power draw'],
            ['show-net', 'Network'],
        ]) {
            const row = new Adw.SwitchRow({title});
            settings.bind(key, row, 'active', Gio.SettingsBindFlags.DEFAULT);
            shown.add(row);
        }

        const calm = new Adw.PreferencesGroup({title: 'Calm'});
        page.add(calm);
        const dim = new Adw.SwitchRow({
            title: 'Fade into the panel while healthy',
            subtitle: 'Line styles only. The cluster is always legible.',
        });
        settings.bind('dim-when-healthy', dim, 'active', Gio.SettingsBindFlags.DEFAULT);
        calm.add(dim);
    }
}
