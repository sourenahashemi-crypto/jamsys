/* Preferences for the JamSys corner readout. Presentation only — nothing here
 * affects what is monitored, which is entirely the daemon's business. */

import Adw from 'gi://Adw';
import Gio from 'gi://Gio';
import Gtk from 'gi://Gtk';

import {ExtensionPreferences} from 'resource:///org/gnome/Shell/Extensions/js/extensions/prefs.js';

export default class JamSysPrefs extends ExtensionPreferences {
    fillPreferencesWindow(window) {
        const settings = this.getSettings();
        const page = new Adw.PreferencesPage({title: 'Readout'});
        window.add(page);

        const placement = new Adw.PreferencesGroup({
            title: 'Placement',
            description: 'Bottom positions float above the desktop rather than living in '
                       + 'the panel, because GNOME Shell has no bottom panel. They can '
                       + 'overlap a dock and are hidden by fullscreen windows.',
        });
        page.add(placement);

        const positions = ['top-left', 'top-center', 'top-right', 'bottom-left', 'bottom-right'];
        const labels = ['Top left', 'Top centre', 'Top right',
                        'Bottom left (floating)', 'Bottom right (floating)'];
        const posRow = new Adw.ComboRow({
            title: 'Position',
            model: Gtk.StringList.new(labels),
        });
        posRow.set_selected(Math.max(0, positions.indexOf(settings.get_string('position'))));
        posRow.connect('notify::selected', r =>
            settings.set_string('position', positions[r.get_selected()]));
        placement.add(posRow);

        const modes = ['compact', 'minimal'];
        const modeRow = new Adw.ComboRow({
            title: 'Layout',
            subtitle: 'Compact is one line; minimal stacks each reading',
            model: Gtk.StringList.new(['Compact (single line)', 'Minimal (stacked)']),
        });
        modeRow.set_selected(Math.max(0, modes.indexOf(settings.get_string('mode'))));
        modeRow.connect('notify::selected', r =>
            settings.set_string('mode', modes[r.get_selected()]));
        placement.add(modeRow);

        const shown = new Adw.PreferencesGroup({
            title: 'What to show while healthy',
            description: 'When something is abnormal the readout replaces all of this '
                       + 'with the one thing that is wrong.',
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
            subtitle: 'The readout should be almost invisible when there is nothing to say',
        });
        settings.bind('dim-when-healthy', dim, 'active', Gio.SettingsBindFlags.DEFAULT);
        calm.add(dim);
    }
}
