#!/usr/bin/env gjs
/* Contract test: does extension.js call the gnome-shell APIs the way GNOME 50
 * actually accepts them?
 *
 * `Params.parse` in the Shell rejects unknown keys outright. An extension that passes
 * one loads fine under a permissive mock and then fails only inside the compositor —
 * where a module cannot be reloaded without logging out, so each attempt costs a whole
 * session. This runs the real `enable()`/`disable()` against strict stubs instead.
 *
 *   gjs -m gnome-extension/tests/shell-api-test.js
 */
import GLib from 'gi://GLib';

let failures = 0;
const ok = (c, what, extra = '') => {
    print(`  ${c ? 'ok  ' : 'FAIL'} ${what}${c ? '' : '  ' + extra}`);
    if (!c) failures++;
};

// A settings object good enough for the extension's reads.
function fakeSettings(overrides = {}) {
    const v = {
        position: 'bottom-right', mode: 'cluster', style: 'cutout',
        scale: 1.0, opacity: 0.92, margin: 14,
        'show-cpu': true, 'show-temp': true, 'show-ram': true, 'show-gpu': true,
        'show-power': true, 'show-net': true, 'dim-when-healthy': true, ...overrides,
    };
    return {
        get_string(k) {
            // Strict on purpose. A key the schema does not have must fail loudly
            // here rather than return undefined and quietly take a default branch,
            // which is how the cut-out face first passed its own test without ever
            // reading the setting that selects it.
            if (!(k in v)) throw new Error(`settings.get_string: unknown key '${k}'`);
            return v[k];
        },
        get_double: k => v[k], get_int: k => v[k],
        get_boolean: k => v[k], set_string(k, x) { v[k] = x; },
        set_double(k, x) { v[k] = x; },
        connect: () => 1, disconnect() {},
        _values: v,
    };
}

// Rewrite the Shell's resource: imports to the strict stubs, then load the real file.
// `new Error().fileName` is a file:// URI in GJS; strip the scheme before using it
// as a path.
const selfUri = new Error().fileName ?? '';
const here = GLib.path_get_dirname(selfUri.replace(/^file:\/\//, ''));
const extPath = `${here}/../jamsys@jamsys.org/extension.js`;
const [okRead, bytes] = GLib.file_get_contents(extPath);
ok(okRead, 'extension.js is readable');
let src = new TextDecoder().decode(bytes);
// The rewritten copy is written *into the extension directory* so its own relative
// imports of format.js and gauges.js resolve exactly as they do in the Shell; only
// the Shell's own modules are redirected, to the strict stubs.
const stubs = `${here}/shellstubs`;
const MAP = {
    "resource:///org/gnome/shell/extensions/extension.js": `${stubs}/extension.js`,
    "resource:///org/gnome/shell/ui/main.js":              `${stubs}/main.js`,
    "resource:///org/gnome/shell/ui/panelMenu.js":         `${stubs}/panelMenu.js`,
    "resource:///org/gnome/shell/ui/popupMenu.js":         `${stubs}/popupMenu.js`,
    "gi://St":      `${stubs}/st.js`,
    "gi://Clutter": `${stubs}/clutter.js`,
    "gi://GObject": `${stubs}/gobject.js`,
};
for (const [a, b] of Object.entries(MAP)) src = src.replaceAll(`'${a}'`, `'file://${b}'`);
// Collect logError calls rather than letting them print. A passing test that emits a
// scary JS ERROR teaches people to ignore test output.
globalThis.__jamsysLogged = [];
globalThis.logError = e => globalThis.__jamsysLogged.push(String(e));
src = '' + src;

const tmp = `${here}/../jamsys@jamsys.org/.shell-api-tmp.js`;
GLib.file_set_contents(tmp, src);

const mod = await import(`file://${tmp}`);
GLib.unlink(tmp);

const Main = await import(`file://${stubs}/main.js`);
const Ext = mod.default;
ok(typeof Ext === 'function', 'extension module exports a class');

function run(label, settings, check) {
    Main.layoutManager.chrome.length = 0;
    Main.panel.statusArea = {};
    const e = Object.create(Ext.prototype);
    e.uuid = 'jamsys@jamsys.org';
    e.getSettings = () => settings;
    try {
        e.enable();
        check(e);
        e.disable();
        ok(true, `${label}: enable() and disable() complete`);
    } catch (err) {
        ok(false, `${label}: threw`, err.message);
        try { e.disable(); } catch { /* best effort */ }
    }
}

print('cluster mode uses addChrome with only the parameters GNOME 50 accepts');
run('cluster', fakeSettings({mode: 'cluster', position: 'bottom-right'}), () => {
    ok(Main.layoutManager.chrome.length === 1, 'exactly one chrome actor added');
    ok(Object.keys(Main.panel.statusArea).length === 0, 'nothing added to the panel');
});

print('\nevery corner places without error');
for (const pos of ['top-left', 'top-center', 'top-right', 'bottom-left', 'bottom-right'])
    run(`cluster ${pos}`, fakeSettings({mode: 'cluster', position: pos}), () => {});

print('\npanel line modes go into the panel, not onto the desktop');
run('compact top-right', fakeSettings({mode: 'compact', position: 'top-right'}), () => {
    ok(Main.layoutManager.chrome.length === 0, 'no chrome actor for a panel mode');
    ok(Object.keys(Main.panel.statusArea).length === 1, 'one status-area item');
});
run('minimal top-left', fakeSettings({mode: 'minimal', position: 'top-left'}), () => {});

print('\na panel mode in a bottom position still floats');
run('compact bottom-left', fakeSettings({mode: 'compact', position: 'bottom-left'}), () => {
    ok(Main.layoutManager.chrome.length === 1, 'falls back to a chrome actor');
});

print('\nextremes of the size and opacity range');
for (const [scale, opacity] of [[0.55, 0.35], [1.8, 1.0]])
    run(`scale ${scale}`, fakeSettings({scale, opacity}), () => {});

print('\nstate can be applied and cleared without a live daemon');
// The face the extension draws is the whole point of this round of work: the
// Shell widget was still rendering the old boxed cluster long after the window
// had moved on, because nothing tied the two together.
run('cut-out face', fakeSettings({style: 'cutout'}), () => {});
run('housing face', fakeSettings({style: 'housing'}), () => {});

run('state', fakeSettings(), e => {
    e._apply(JSON.stringify({
        health: 'critical', cpu_pct: 99, cpu_temp_c: 97, mem_pct: 90, gpu_pct: 50,
        power_w: 54, battery_pct: 8, on_battery: true, has_battery: true,
        net_ok: false, dgpu: 'active', alert_title: 'CPU is running critically hot',
        alert_subsystem: 'CPU', alert_severity: 3, open_alerts: 2,
    }));
    ok(true, 'a critical state is accepted');
    const before = globalThis.__jamsysLogged.length;
    e._apply('not json at all');
    ok(globalThis.__jamsysLogged.length > before,
       'malformed JSON is logged, not thrown, out of _apply');
    e._onVanished();
    ok(true, 'the daemon disappearing is handled');
});


/* ------------------------------------------------------- scroll to resize */
/* The corner widget could only be resized from the preferences dialog, which is
 * not where anyone looks when a gadget is the wrong size. These drive the real
 * handler on the real widget; the interesting case is the one Wayland sends. */

const Clutter = (await import(`file://${stubs}/clutter.js`)).default;
const D = Clutter.ScrollDirection;
const scrollEvent = (direction, dy = 0) => ({
    get_scroll_direction: () => direction,
    get_scroll_delta: () => [0, dy],
});

print('\nscroll to resize');

function withCluster(settings, fn) {
    Main.layoutManager.chrome.length = 0;
    Main.panel.statusArea = {};
    const e = Object.create(Ext.prototype);
    e.uuid = 'jamsys@jamsys.org';
    e.getSettings = () => settings;
    e.enable();
    try {
        fn(e._widget, settings);
    } finally {
        e.disable();
    }
}

withCluster(fakeSettings({scale: 1.0}), (c, st) => {
    c._onScroll(scrollEvent(D.UP));
    ok(st._values.scale > 1.0, 'scroll up grows the widget', `got ${st._values.scale}`);
    const grown = st._values.scale;
    c._onScroll(scrollEvent(D.DOWN));
    ok(st._values.scale < grown, 'scroll down shrinks it again');
});

// Wayland sends SMOOTH, never UP/DOWN. Handling only the discrete directions
// would mean the feature silently does nothing on the compositor this targets.
withCluster(fakeSettings({scale: 1.0}), (c, st) => {
    c._onScroll(scrollEvent(D.SMOOTH, -1.0));
    ok(st._values.scale > 1.0, 'smooth scroll up grows it — this is the Wayland path',
       `got ${st._values.scale}`);
    const grown = st._values.scale;
    c._onScroll(scrollEvent(D.SMOOTH, 1.0));
    ok(st._values.scale < grown, 'smooth scroll down shrinks it');
});

withCluster(fakeSettings({scale: 1.0}), (c, st) => {
    c._onScroll(scrollEvent(D.SMOOTH, 0));
    ok(st._values.scale === 1.0, 'a zero-delta smooth event changes nothing');
    c._onScroll(scrollEvent(D.LEFT));
    ok(st._values.scale === 1.0, 'horizontal scroll changes nothing');
});

// Clamping must agree with the gschema range. If it does not, GSettings clamps the
// write silently and the widget looks like it has stopped responding.
withCluster(fakeSettings({scale: 3.0}), (c, st) => {
    for (let i = 0; i < 20; i++) c._onScroll(scrollEvent(D.UP));
    ok(st._values.scale <= 3.0, 'never exceeds the schema maximum', `got ${st._values.scale}`);
});
withCluster(fakeSettings({scale: 0.55}), (c, st) => {
    for (let i = 0; i < 20; i++) c._onScroll(scrollEvent(D.DOWN));
    ok(st._values.scale >= 0.55, 'never drops below the schema minimum', `got ${st._values.scale}`);
});

print(`\n${failures === 0 ? 'ALL PASS' : failures + ' FAILURE(S)'}`);
imports.system.exit(failures === 0 ? 0 : 1);

