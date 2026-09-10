#!/usr/bin/env gjs
/* Unit tests for the corner readout's rendering, runnable without gnome-shell.
 *
 * Run:  gjs -m gnome-extension/tests/format-test.js
 */
import {labelFor, normalLine, shortAlert, styleFor, pct, deg, watt}
    from '../jamsys@jamsys.org/format.js';

let failures = 0;
function eq(actual, expected, what) {
    if (actual !== expected) {
        print(`  FAIL ${what}\n       got:      ${JSON.stringify(actual)}` +
              `\n       expected: ${JSON.stringify(expected)}`);
        failures++;
    } else {
        print(`  ok   ${what}`);
    }
}
function ok(cond, what) { eq(!!cond, true, what); }

const ALL = {
    'show-cpu': true, 'show-temp': true, 'show-ram': true,
    'show-gpu': true, 'show-power': true, 'show-net': true,
};

const healthy = {
    health: 'healthy', cpu_pct: 7.4, cpu_temp_c: 52.3, mem_pct: 18.2, gpu_pct: 0,
    power_w: 11.24, battery_pct: 87, on_battery: true, has_battery: true,
    net_ok: true, net_label: '-56 dBm', dgpu: 'suspended',
    alert_title: '', alert_detail: '', alert_expected: '', alert_subsystem: '',
    alert_severity: 0, open_alerts: 0,
};

print('rounding');
eq(pct(7.4), '7%', 'percent rounds to whole numbers');
eq(deg(52.3), '52°', 'temperature rounds to whole degrees');
eq(watt(11.24), '11.2W', 'power keeps one decimal');
eq(watt(-33.8), '33.8W', 'charging power is shown as a magnitude');

print('\ncompact line while healthy');
eq(labelFor(healthy, ALL, false),
   'CPU 7% · 52° | RAM 18% | GPU — | 11.2W | NET ✓',
   'matches the single-line form from the specification');

print('\nminimal (stacked) mode');
eq(labelFor(healthy, ALL, true),
   'CPU 7% · 52°\nRAM 18%\nGPU —\n11.2W\nNET ✓',
   'one reading per line');

print('\na suspended discrete GPU is a dash, not a zero');
ok(labelFor(healthy, ALL, false).includes('GPU —'),
   'suspended reads as unavailable rather than 0%');
const awake = {...healthy, dgpu: 'active', gpu_pct: 34};
ok(labelFor(awake, ALL, false).includes('GPU 34%'), 'an awake GPU shows its utilisation');

print('\nhiding readings');
eq(labelFor(healthy, {...ALL, 'show-gpu': false, 'show-net': false}, false),
   'CPU 7% · 52° | RAM 18% | 11.2W',
   'disabled readings disappear entirely');
eq(labelFor(healthy, {...ALL, 'show-temp': false}, false),
   'CPU 7% | RAM 18% | GPU — | 11.2W | NET ✓',
   'temperature can be dropped without breaking the CPU field');

print('\non AC');
const ac = {...healthy, on_battery: false, battery_pct: 91};
ok(labelFor(ac, ALL, false).includes('AC 91%'), 'shows charge rather than a discharge rate');

print('\nno battery at all');
const desktop = {...healthy, has_battery: false, on_battery: false};
ok(!labelFor(desktop, ALL, false).includes('W'), 'a desktop shows no power field');

print('\nabnormal state replaces the readings');
const powerAlert = {
    ...healthy, health: 'attention', power_w: 29.8,
    alert_title: 'Unusually high power draw while idle',
    alert_subsystem: 'Power', alert_severity: 1,
};
eq(labelFor(powerAlert, ALL, false), '⚠ POWER 29.8W',
   'only the abnormal metric is shown');
ok(!labelFor(powerAlert, ALL, false).includes('RAM'),
   'healthy readings are suppressed while something is wrong');

const critical = {
    ...healthy, health: 'critical', cpu_pct: 99, cpu_temp_c: 97,
    alert_title: 'CPU is running critically hot', alert_subsystem: 'CPU', alert_severity: 3,
};
eq(labelFor(critical, ALL, false), '✕ CPU 99% 97°', 'critical uses a distinct marker');

print('\nnetwork alert names the interface state');
const net = {...healthy, health: 'attention', net_ok: false, net_label: 'offline',
             alert_title: 'Wi-Fi disconnected', alert_subsystem: 'Network'};
eq(labelFor(net, ALL, false), '⚠ NETWORK offline', 'network alert reads plainly');

print('\nsubsystems with no single number use the alert count');
const svc = {...healthy, health: 'attention', open_alerts: 1,
             alert_title: 'snap.openshell.gateway.service has failed',
             alert_subsystem: 'Services'};
eq(labelFor(svc, ALL, false), '⚠ SERVICES 1', 'a bare subsystem name is not enough');
const svc3 = {...svc, open_alerts: 3};
eq(labelFor(svc3, ALL, false), '⚠ SERVICES 3', 'the count is the measurement');

print('\nan alert with no recognised subsystem still renders');
const odd = {...healthy, health: 'attention', alert_title: 'Something', alert_subsystem: ''};
eq(labelFor(odd, ALL, false), '⚠ ATTENTION', 'falls back rather than rendering blank');

print('\nstyle classes');
eq(styleFor('healthy'), 'jamsys-healthy', 'healthy');
eq(styleFor('attention'), 'jamsys-attention', 'attention');
eq(styleFor('critical'), 'jamsys-critical', 'critical');
eq(styleFor('nonsense'), 'jamsys-unknown', 'an unknown health word is not treated as healthy');

print(`\n${failures === 0 ? 'ALL PASS' : failures + ' FAILURE(S)'}`);
imports.system.exit(failures === 0 ? 0 : 1);
