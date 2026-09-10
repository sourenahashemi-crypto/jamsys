#!/usr/bin/env gjs
/* End-to-end check of the exact D-Bus path the Shell extension uses.
 *
 * GNOME Shell on Wayland cannot load a newly-installed extension without a session
 * restart, so this harness stands in for it: same interface XML, same
 * `Gio.DBusProxy.makeProxyWrapper`, same `StateChanged` subscription, same JSON
 * parsing, same rendering module. If this passes, the extension's data path works.
 *
 * Run:  gjs -m gnome-extension/tests/live-dbus-test.js
 */
import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import {labelFor, styleFor} from '../jamsys@jamsys.org/format.js';

const BUS_NAME = 'org.jamsys.Daemon';
const OBJECT_PATH = '/org/jamsys/Daemon';

const Iface = `
<node>
  <interface name="org.jamsys.Daemon">
    <method name="GetState"><arg type="s" direction="out" name="state"/></method>
    <method name="GetVersion"><arg type="s" direction="out" name="version"/></method>
    <method name="AckTopAlert"><arg type="b" direction="out" name="ok"/></method>
    <signal name="StateChanged"><arg type="s" name="state"/></signal>
  </interface>
</node>`;

const Proxy = Gio.DBusProxy.makeProxyWrapper(Iface);
const ALL = {'show-cpu': true, 'show-temp': true, 'show-ram': true,
             'show-gpu': true, 'show-power': true, 'show-net': true};

let failures = 0;
const ok = (c, what) => { print(`  ${c ? 'ok  ' : 'FAIL'} ${what}`); if (!c) failures++; };

const loop = new GLib.MainLoop(null, false);
let proxy;
try {
    proxy = new Proxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH);
} catch (e) {
    print(`  FAIL could not create the proxy: ${e.message}`);
    imports.system.exit(1);
}

print('proxy and version');
const [version] = proxy.GetVersionSync();
ok(typeof version === 'string' && version.length > 0, `GetVersion -> ${version}`);

print('\nGetState returns parseable JSON with everything the widget needs');
const [json] = proxy.GetStateSync();
let s;
try {
    s = JSON.parse(json);
    ok(true, 'JSON parses');
} catch (e) {
    ok(false, `JSON parse failed: ${e.message}`);
    imports.system.exit(1);
}
for (const k of ['health', 'cpu_pct', 'cpu_temp_c', 'mem_pct', 'gpu_pct', 'power_w',
                 'battery_pct', 'on_battery', 'has_battery', 'net_ok', 'net_label',
                 'dgpu', 'alert_title', 'alert_subsystem', 'alert_severity', 'open_alerts'])
    ok(k in s, `field present: ${k}`);

print('\nvalues are plausible for a real machine');
ok(['healthy', 'attention', 'critical', 'unknown'].includes(s.health), `health = ${s.health}`);
ok(s.cpu_pct >= 0 && s.cpu_pct <= 100, `cpu_pct = ${s.cpu_pct.toFixed(1)}`);
ok(s.mem_pct >= 0 && s.mem_pct <= 100, `mem_pct = ${s.mem_pct.toFixed(1)}`);
ok(s.cpu_temp_c > 0 && s.cpu_temp_c < 120, `cpu_temp_c = ${s.cpu_temp_c}`);
ok(['active', 'suspended', 'suspending', 'resuming', 'unknown', ''].includes(s.dgpu),
   `dgpu = ${s.dgpu}`);

print('\nthe label the panel would actually show');
const label = labelFor(s, ALL, false);
print(`       "${label}"   [${styleFor(s.health)}]`);
ok(label.length > 0 && label.length < 120, 'label is a sensible length');

print('\nStateChanged arrives without polling');
let got = 0;
const id = proxy.connectSignal('StateChanged', (_p, _sender, [payload]) => {
    got++;
    if (got === 1) {
        try {
            const st = JSON.parse(payload);
            print(`       push #1: "${labelFor(st, ALL, false)}"`);
            ok(true, 'first push parsed and rendered');
        } catch (e) {
            ok(false, `push payload did not parse: ${e.message}`);
        }
    }
    if (got >= 2) loop.quit();
});
// The daemon only emits when a displayed value materially changes, so allow enough
// time for two such changes on a live machine.
GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 40, () => { loop.quit(); return false; });
loop.run();
proxy.disconnectSignal(id);
ok(got >= 1, `received ${got} StateChanged signal(s) without polling`);

print(`\n${failures === 0 ? 'ALL PASS' : failures + ' FAILURE(S)'}`);
imports.system.exit(failures === 0 ? 0 : 1);
