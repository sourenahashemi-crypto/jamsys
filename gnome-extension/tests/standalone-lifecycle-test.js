#!/usr/bin/env gjs
// Exercise the window's daemon wiring without creating a window or changing settings.
import GLib from 'gi://GLib';

let failures = 0;
function ok(condition, message) {
    print(`  ${condition ? 'ok  ' : 'FAIL'} ${message}`);
    if (!condition) failures++;
}
const here = GLib.path_get_dirname(new Error().fileName.replace(/^file:\/\//, ''));
const [, bytes] = GLib.file_get_contents(`${here}/../jamsys@jamsys.org/standalone.js`);
const source = new TextDecoder().decode(bytes);
const daemonCode = source.slice(source.indexOf('function connectDaemon()'), source.lastIndexOf('app.run([])'));
let vanished = null;
let appeared = null;
let unwatched = false;
const proxies = [];
class Proxy {
    constructor() { proxies.push(this); }
    connectSignal(name, callback) {
        if (name !== 'StateChanged') throw new Error(`unexpected signal ${name}`);
        this.signal = callback;
        return 1;
    }
    GetStateRemote(callback) { this.reply = callback; }
    disconnectSignal(id) {
        if (id !== 1 || this.disconnected) throw new Error('invalid signal disconnect');
        this.disconnected = true;
    }
}
const gio = {
    DBus: {session: {}}, BusType: {SESSION: 0}, BusNameWatcherFlags: {NONE: 0},
    bus_watch_name(_type, _name, _flags, found, lost) {
        vanished = lost;
        appeared = found;
        found(null, 'org.jamsys.Daemon', ':1.23');
        return 1;
    },
    bus_unwatch_name(id) {
        if (id !== 1 || unwatched) throw new Error('invalid watcher disconnect');
        unwatched = true;
    },
};
const harness = new Function('Gio', 'GLib', 'Proxy', `
    const BUS_NAME = 'org.jamsys.Daemon', OBJECT_PATH = '/org/jamsys/Daemon';
    let proxy = null, state = null;
    let daemonWatch = 0, daemonSignal = 0, daemonGeneration = 0;
    const area = {queue_draw() {}};
    ${daemonCode}
    return {connectDaemon, stopDaemon, getState: () => state};
`)(gio, {timeout_add_seconds() { throw new Error('unexpected retry timer'); }}, Proxy);
harness.connectDaemon();
proxies[0].reply([JSON.stringify({health: 'healthy', cpu_pct: 77})], null);
ok(harness.getState()?.cpu_pct === 77, 'the initial reading arrives');
ok(vanished !== null, 'the window watches for the daemon disappearing');
vanished?.();
ok(harness.getState() === null, 'daemon loss clears old readings immediately');
ok(proxies[0].disconnected, 'old signal subscription is disconnected');
proxies[0].reply([JSON.stringify({cpu_pct: 88})], null);
ok(harness.getState() === null, 'an old reply cannot restore stale readings');
appeared();
ok(proxies.length === 2, 'daemon restart creates a fresh proxy');
proxies[1].signal(null, null, [JSON.stringify({cpu_pct: 22})]);
proxies[1].reply([JSON.stringify({cpu_pct: 11})], null);
ok(harness.getState()?.cpu_pct === 22, 'a delayed initial reply cannot overwrite a newer push');
vanished();
appeared();
try {
    // Match replyFunc([], error, null) in GJS's actual Gio override.
    proxies[2].reply([], new Error('service unavailable'));
    ok(harness.getState() === null, 'a failed initial call keeps the window offline');
} catch (e) {
    ok(false, `a failed initial call must not throw: ${e.message}`);
}
harness.stopDaemon();
ok(unwatched && proxies[2].disconnected, 'closing the window releases watch and signal');
proxies[2].signal(null, null, [JSON.stringify({cpu_pct: 99})]);
ok(harness.getState() === null, 'queued callbacks after close are ignored');
print(`${failures} failure(s)`);
imports.system.exit(failures ? 1 : 0);
