#!/usr/bin/env gjs
/* Do the Clutter APIs the stubs model actually exist on this machine?
 *
 * shell-api-test.js runs the extension against stubs. A stub can only be as
 * right as whoever wrote it: the Clutter stub kept `DragAction` and the event
 * stub kept `get_click_count()` after GNOME 50 removed both, so every test
 * passed for an extension that GNOME Shell put straight into ERROR.
 *
 * This loads the real Mutter typelibs (no compositor needed) and checks every
 * Clutter name the extension and its stubs rely on, then checks the settings
 * the extension writes against the real compiled schema.
 *
 *   gjs -m gnome-extension/tests/real-api-test.js
 */
import GLib from 'gi://GLib';
import Gio from 'gi://Gio';
import GObject from 'gi://GObject';
import GIRepository from 'gi://GIRepository';

let failures = 0;
const ok = (c, what, extra = '') => {
    print(`  ${c ? 'ok  ' : 'FAIL'} ${what}${c ? '' : '  ' + extra}`);
    if (!c) failures++;
};
const here = GLib.path_get_dirname((new Error().fileName ?? '').replace(/^file:\/\//, ''));
const extDir = `${here}/../jamsys@jamsys.org`;

/* -- Clutter ------------------------------------------------------------ */
function mutterDirs() {
    const out = [];
    for (const base of ['/usr/lib/x86_64-linux-gnu', '/usr/lib64', '/usr/lib']) {
        const d = Gio.File.new_for_path(base);
        let en;
        try {
            en = d.enumerate_children('standard::name', 0, null);
        } catch {
            continue;
        }
        let info;
        while ((info = en.next_file(null)))
            if (/^mutter-\d+$/.test(info.get_name())) out.push(`${base}/${info.get_name()}`);
    }
    return out;
}

let Clutter = null;
const dirs = mutterDirs();
try {
    const repo = GIRepository.Repository.dup_default?.() ?? GIRepository.Repository;
    for (const d of dirs) {
        repo.prepend_search_path(d);
        repo.prepend_library_path(d);
    }
    Clutter = (await import('gi://Clutter')).default;
} catch (e) {
    print(`  skip Clutter typelib not loadable here (${e.message})`);
}

if (Clutter) {
    print(`Clutter from ${dirs.join(', ')}`);
    const [, bytes] = GLib.file_get_contents(`${extDir}/extension.js`);
    const src = new TextDecoder().decode(bytes);

    // Every Clutter.<Name> the extension mentions outside a comment.
    const code = src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/\/\/.*$/gm, '');
    const names = [...new Set([...code.matchAll(/\bClutter\.([A-Za-z_]+)/g)].map(m => m[1]))];
    for (const n of names)
        ok(Clutter[n] !== undefined, `Clutter.${n} exists`);

    for (const m of ['get_button', 'get_scroll_direction', 'get_scroll_delta'])
        ok(typeof Clutter.Event.prototype[m] === 'function', `Clutter.Event has ${m}()`);
    for (const m of [...new Set([...code.matchAll(/\bevent\.([a-z_]+)\(/g)].map(x => x[1]))])
        ok(typeof Clutter.Event.prototype[m] === 'function',
           `event.${m}() used by the extension is real`);

    // The stub's claims about gestures, held against the real classes.
    const stub = (await import(`file://${here}/shellstubs/clutter.js`)).default;
    for (const cls of ['ClickGesture', 'PanGesture']) {
        const real = Clutter[cls];
        const props = new Set();
        const sigs = new Set();
        for (let t = real.$gtype; t && t.name !== 'GObject'; t = GObject.type_parent(t)) {
            for (const id of GObject.signal_list_ids(t)) sigs.add(GObject.signal_name(id));
        }
        for (const p of GObject.Object.list_properties.call(real))
            props.add(p.name.replaceAll('-', '_'));
        for (const p of stub[cls].PROPS)
            ok(props.has(p), `${cls} really has the property ${p}`);
        for (const s of stub[cls].SIGNALS)
            ok(sigs.has(s), `${cls} really emits ${s}`);
    }
    ok(typeof Clutter.PanGesture.prototype.get_accumulated_delta === 'function',
       'PanGesture.get_accumulated_delta() is real');
}

/* -- the schema, compiled and written to ---------------------------------- */
const tmp = GLib.dir_make_tmp('jamsys-schema-XXXXXX');
try {
    const p = Gio.Subprocess.new(
        ['glib-compile-schemas', '--strict', `--targetdir=${tmp}`, `${extDir}/schemas`],
        Gio.SubprocessFlags.STDERR_PIPE);
    const [, , err] = p.communicate_utf8(null, null);
    ok(p.get_successful(), 'the schema compiles under --strict', err);
    if (p.get_successful()) {
        const source = Gio.SettingsSchemaSource.new_from_directory(tmp, null, false);
        const schema = source.lookup('org.gnome.shell.extensions.jamsys', false);
        const s = new Gio.Settings({
            settings_schema: schema, backend: Gio.memory_settings_backend_new()});
        ok(s.set_string('position', 'custom') && s.get_string('position') === 'custom',
           "a dragged widget's position='custom' is accepted");
        ok(s.set_string('mode', 'compact'), "Hide's mode='compact' is accepted");
        ok(!s.set_string('style', 'nonsense') && s.get_string('style') === 'cutout',
           'an unknown face is refused');
        const [, eb] = GLib.file_get_contents(`${extDir}/extension.js`);
        const es = new TextDecoder().decode(eb);
        const lo = Number(es.match(/SCALE_MIN = ([\d.]+)/)[1]);
        const hi = Number(es.match(/SCALE_MAX = ([\d.]+)/)[1]);
        ok(s.set_double('scale', lo) && s.set_double('scale', hi),
           `the scroll clamp ${lo}..${hi} is inside the schema range`);
        ok(!s.set_double('scale', hi + 0.01) && !s.set_double('scale', lo - 0.01),
           'and is exactly the schema range, not narrower');
    }
} catch (e) {
    print(`  skip glib-compile-schemas not available (${e.message})`);
} finally {
    for (const f of ['gschemas.compiled']) GLib.unlink(`${tmp}/${f}`);
    GLib.rmdir(tmp);
}

print(failures ? `\n${failures} FAILED` : '\nall real-API checks passed');
imports.system.exit(failures ? 1 : 0);
