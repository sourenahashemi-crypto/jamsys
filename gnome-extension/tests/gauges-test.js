#!/usr/bin/env gjs
/* Tests for the instrument cluster's value mapping and geometry helpers.
 *
 * The drawing itself is reviewed visually via tests/render-cluster.js; what is
 * asserted here is the arithmetic that decides where a needle points, which is the
 * part that can be wrong without looking wrong.
 *
 *   gjs -m gnome-extension/tests/gauges-test.js
 */
import {norm, readings, A0, SWEEP, CLUSTER_W, CLUSTER_H}
    from '../jamsys@jamsys.org/gauges.js';

let failures = 0;
const eq = (a, b, what) => {
    const ok = Object.is(a, b) || (typeof a === 'number' && Math.abs(a - b) < 1e-9);
    print(`  ${ok ? 'ok  ' : 'FAIL'} ${what}${ok ? '' : `  (got ${a}, want ${b})`}`);
    if (!ok) failures++;
};
const ok = (c, what) => eq(!!c, true, what);

print('needle mapping');
eq(norm(0, 0, 100), 0, '0% pins the needle at the start of the sweep');
eq(norm(100, 0, 100), 1, '100% pins it at the end');
eq(norm(50, 0, 100), 0.5, 'midscale');
eq(norm(-20, 0, 100), 0, 'below scale is clamped, not wrapped');
eq(norm(400, 0, 100), 1, 'above scale is clamped');
eq(norm(NaN, 0, 100), 0, 'a NaN reading parks the needle rather than throwing');
eq(norm(Infinity, 0, 100), 0, 'so does an infinity');
eq(norm(5, 10, 10), 0, 'a degenerate range does not divide by zero');

print('\nsweep geometry');
ok(Math.abs(SWEEP - Math.PI * 1.5) < 1e-9, '270° sweep, the automotive convention');
ok(Math.abs(A0 - Math.PI * 0.75) < 1e-9, 'starting at 135°, so the gap is at the bottom');
// The empty sector must be centred on straight down, which is where captions go.
const endAng = (A0 + SWEEP) % (Math.PI * 2);
ok(Math.abs(endAng - Math.PI * 0.25) < 1e-9, 'ending at 45°');

print('\nreadings from a healthy laptop');
const base = {
    health: 'healthy', cpu_pct: 7.4, cpu_temp_c: 52, mem_pct: 18.2, gpu_pct: 0,
    power_w: 11.24, battery_pct: 87, on_battery: true, has_battery: true,
    net_ok: true, dgpu: 'suspended', alert_subsystem: '',
};
let r = readings(base);
eq(r.cpu.text, '7', 'CPU rounds to a whole percent');
eq(r.cpu.sub, '52°C', 'temperature is the secondary reading');
eq(r.ram.text, '18', 'RAM rounds');
eq(r.power.text, '11.2', 'power keeps one decimal');
ok(r.power.known, 'a battery means the power figure is real');
ok(!r.power.charging, 'discharging is not charging');
eq(r.battery.pct, 87, 'battery percent');

print('\na suspended discrete GPU is not zero, it is unknown');
eq(r.gpu.text, '—', 'shows a dash rather than 0%');
ok(r.gpu.dim, 'and the dial is dimmed');
eq(r.gpu.v, 0, 'the needle parks at the start');

print('\nan awake GPU reads normally');
r = readings({...base, dgpu: 'active', gpu_pct: 82});
eq(r.gpu.text, '82', 'utilisation is shown');
ok(!r.gpu.dim, 'and the dial is lit');
ok(Math.abs(r.gpu.v - 0.82) < 1e-9, 'needle at 82%');

print('\ntemperature earns its own colour');
const cold = readings({...base, cpu_temp_c: 45});
const warm = readings({...base, cpu_temp_c: 80});
const hot  = readings({...base, cpu_temp_c: 95});
ok(cold.cpu.subColor !== warm.cpu.subColor, '45 °C and 80 °C differ');
ok(warm.cpu.subColor !== hot.cpu.subColor, '80 °C and 95 °C differ');

print('\ncharging is distinguishable from draining');
r = readings({...base, on_battery: false, power_w: -38.2});
ok(r.power.charging, 'on AC with a battery reads as charging');
eq(r.power.text, '38.2', 'the magnitude is shown, not a negative number');

print('\na machine with no battery says so instead of showing zero watts');
r = readings({...base, has_battery: false, on_battery: false, power_w: 0});
eq(r.power.text, '—', 'no invented zero');
ok(!r.power.known, 'and the field is flagged unknown');

print('\nfull-scale power is chosen for this class of machine');
r = readings({...base, power_w: 60});
eq(r.power.v, 1, '60 W is full scale');
r = readings({...base, power_w: 11.24});
ok(r.power.v > 0.1 && r.power.v < 0.3,
   'an ordinary idle draw sits low but off the stop, not pinned at zero');

print('\ncluster proportions');
ok(CLUSTER_W > CLUSTER_H * 1.8, 'wide and low, like an instrument binnacle');

print(`\n${failures === 0 ? 'ALL PASS' : failures + ' FAILURE(S)'}`);
imports.system.exit(failures === 0 ? 0 : 1);
