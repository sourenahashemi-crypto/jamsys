#!/usr/bin/env gjs
/* Render the instrument cluster to a PNG using the *same* drawing code the Shell
 * widget uses. Inside gnome-shell the Cairo context comes from an St.DrawingArea
 * repaint; here it comes from an ImageSurface. Nothing else differs.
 *
 * This exists because the extension cannot be loaded without a session restart, and
 * a design nobody has looked at is not a design.
 *
 *   gjs -m gnome-extension/tests/render-cluster.js OUTDIR [--live] [--scale N]
 */
import Gio from 'gi://Gio';
import {drawCluster, CLUSTER_W, CLUSTER_H} from '../jamsys@jamsys.org/gauges.js';

const Cairo = imports.cairo;
const argv = ARGV ?? [];
const outDir = argv.find(a => !a.startsWith('--')) ?? '/tmp';
const live = argv.includes('--live');
const si = argv.indexOf('--scale');
const scale = si >= 0 ? parseFloat(argv[si + 1]) : 2;   // 2 = retina, easier to review

function render(name, state, opts = {}) {
    const w = Math.round(CLUSTER_W * scale);
    const h = Math.round(CLUSTER_H * scale);
    const surf = new Cairo.ImageSurface(Cairo.Format.ARGB32, w, h);
    const cr = new Cairo.Context(surf);
    // A mid-grey ground so panel transparency is visible in the review image the way
    // it would be over a wallpaper.
    cr.setSourceRGBA(0.16, 0.17, 0.19, 1); cr.paint();
    cr.scale(scale, scale);
    drawCluster(cr, CLUSTER_W, CLUSTER_H, state, opts);
    cr.$dispose();
    const p = `${outDir}/cluster-${name}.png`;
    surf.writeToPNG(p);
    print(`  ${name.padEnd(12)} -> ${p}`);
}

const base = {
    health: 'healthy', cpu_pct: 7.4, cpu_temp_c: 52, mem_pct: 18.2, gpu_pct: 0,
    power_w: 11.24, battery_pct: 87, on_battery: true, has_battery: true,
    net_ok: true, net_label: '-56 dBm', dgpu: 'suspended',
    alert_title: '', alert_detail: '', alert_expected: '', alert_subsystem: '',
    alert_severity: 0, open_alerts: 0,
};

if (live) {
    const Iface = `
<node><interface name="org.jamsys.Daemon">
  <method name="GetState"><arg type="s" direction="out" name="state"/></method>
</interface></node>`;
    const P = Gio.DBusProxy.makeProxyWrapper(Iface);
    const p = new P(Gio.DBus.session, 'org.jamsys.Daemon', '/org/jamsys/Daemon');
    const [json] = p.GetStateSync();
    render('live', JSON.parse(json));
} else {
    render('idle', base);
    render('load', {...base, cpu_pct: 94, cpu_temp_c: 88, mem_pct: 71,
                    gpu_pct: 82, dgpu: 'active', power_w: 48.6, battery_pct: 41});
    render('warn', {...base, health: 'attention', cpu_pct: 6, power_w: 29.8,
                    alert_title: 'Unusually high power draw while idle',
                    alert_subsystem: 'Power', alert_severity: 1, open_alerts: 1});
    render('crit', {...base, health: 'critical', cpu_pct: 99, cpu_temp_c: 97,
                    mem_pct: 94, power_w: 54, battery_pct: 8, net_ok: false,
                    dgpu: 'active', gpu_pct: 61,
                    alert_title: 'CPU is running critically hot',
                    alert_subsystem: 'CPU', alert_severity: 3, open_alerts: 2});
    render('ac', {...base, on_battery: false, battery_pct: 100, power_w: -38.2,
                  cpu_pct: 22, mem_pct: 31});
    render('desktop', {...base, has_battery: false, on_battery: false, power_w: 0,
                       cpu_pct: 14, mem_pct: 44, dgpu: 'active', gpu_pct: 3});
}
print('done');
