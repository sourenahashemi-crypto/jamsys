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
let watchCount = 0;
let retryCb = null;
let retrySource = 0;
const proxies = [];
class Proxy {
    // Strict, like the house stubs. A zero-argument constructor let a wrong bus
    // type, bus name and object path all pass unnoticed.
    constructor(bus, name, path) {
        if (!bus) throw new Error('proxy: a bus connection is required');
        // Literals on purpose: the production constants live inside the
        // harness's evaluated scope, and this stub must fail if they change.
        if (name !== 'org.jamsys.Daemon')
            throw new Error(`proxy: wrong bus name ${name}`);
        if (path !== '/org/jamsys/Daemon')
            throw new Error(`proxy: wrong object path ${path}`);
        proxies.push(this);
    }
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
    BusType: {SESSION: 2, SYSTEM: 1},
    BusNameWatcherFlags: {NONE: 0},
    bus_watch_name(type, name, flags, found, lost) {
        // Strict: a wrong bus type, name or flag set must fail here rather than
        // silently watching the wrong thing.
        if (type !== 2) throw new Error(`bus_watch_name: wrong bus type ${type}`);
        if (name !== 'org.jamsys.Daemon')
            throw new Error(`bus_watch_name: wrong name ${name}`);
        if (flags !== 0) throw new Error(`bus_watch_name: wrong flags ${flags}`);
        watchCount++;
        vanished = lost;
        appeared = found;
        found(null, 'org.jamsys.Daemon', ':1.23');
        return watchCount;
    },
    bus_unwatch_name(id) {
        if (id < 1 || id > watchCount) throw new Error('invalid watcher disconnect');
        unwatched = true;
    },
};
const glib = {
    PRIORITY_DEFAULT: 0,
    timeout_add_seconds(_prio, _secs, cb) { retryCb = cb; return ++retrySource; },
    Source: {
        remove(id) {
            if (!id) throw new Error('removing a source that was never armed');
            retryCb = null;
        },
    },
};
const harness = new Function('Gio', 'GLib', 'Proxy', `
    const BUS_NAME = 'org.jamsys.Daemon', OBJECT_PATH = '/org/jamsys/Daemon';
    const RECONNECT_S = 10;
    let proxy = null, state = null;
    let daemonWatch = 0, daemonSignal = 0, daemonGeneration = 0, daemonRetry = 0;
    const area = {queue_draw() {}};
    ${daemonCode}
    return {connectDaemon, stopDaemon, getState: () => state,
            isConnected: () => proxy !== null};
`)(gio, glib, Proxy);
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

// A name watch is not enough on its own. Measured against real GLib: with no
// session bus reachable, bus_watch_name fires vanished exactly once and then
// never again, so a window started before the bus was ready waited for ever.
{
    harness.stopDaemon();
    retryCb = null;
    gio.bus_watch_name = function (type, name, flags, found, lost) {
        if (type !== 2) throw new Error(`wrong bus type ${type}`);
        if (name !== 'org.jamsys.Daemon') throw new Error(`wrong name ${name}`);
        watchCount++;
        vanished = lost;
        appeared = found;
        return watchCount;          // deliberately never calls found()
    };
    harness.connectDaemon();
    ok(!harness.isConnected(), 'nothing is connected when the name never appears');
    ok(typeof retryCb === 'function', 'a bounded retry is armed when nothing connects');
    const armed = watchCount;
    retryCb();
    ok(watchCount === armed + 1, 'the retry re-arms the watch rather than giving up');
    harness.stopDaemon();
    ok(retryCb === null, 'and closing the window cancels the retry');
}

print(`${failures} failure(s)`);
imports.system.exit(failures ? 1 : 0);
