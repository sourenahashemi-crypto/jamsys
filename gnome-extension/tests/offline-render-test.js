#!/usr/bin/env gjs
// Real Cairo rendering, with text bounds checked at the minimum supported sizes.
import {drawUnavailable, CLUSTER_W, CLUSTER_H, BARE_W, BARE_H}
    from '../jamsys@jamsys.org/gauges.js';
const Cairo = imports.cairo;
let failures = 0;
let checks = 0;
function ok(condition, message) {
    checks++;
    print(`  ${condition ? 'ok  ' : 'FAIL'} ${message}`);
    if (!condition) failures++;
}
for (const [face, width, height] of [['housing', CLUSTER_W, CLUSTER_H], ['cutout', BARE_W, BARE_H]]) {
    for (const scale of [0.55, 1.3]) {
        for (const [ground, shade] of [['light', 0.95], ['dark', 0.08]]) {
            const w = Math.round(width * scale), h = Math.round(height * scale);
            const surface = new Cairo.ImageSurface(Cairo.Format.ARGB32, w, h);
            const cr = new Cairo.Context(surface);
            cr.setSourceRGB(shade, shade, shade);
            cr.paint();
            const text = [];
            let x = 0, y = 0;
            const context = new Proxy(cr, {get(target, key) {
                if (key === 'moveTo') return (px, py) => {
                    x = px; y = py; target.moveTo(px, py);
                };
                if (key === 'showText') return value => {
                    const e = target.textExtents(value);
                    ok(x + e.xBearing >= 0 && x + e.xBearing + e.width <= w &&
                       y + e.yBearing >= 0 && y + e.yBearing + e.height <= h,
                       `${face} ${scale} ${ground}: text fits`);
                    text.push(value);
                    target.showText(value);
                };
                return typeof target[key] === 'function' ? target[key].bind(target) : target[key];
            }});
            drawUnavailable(context, w, h);
            ok(text.join('\n') === 'JamSys — waiting for monitoring\nsystemctl --user start jamsysd',
               `${face} ${scale} ${ground}: offline status and actionable command`);
            cr.$dispose();
            if (ARGV[0]) surface.writeToPNG(`${ARGV[0]}/offline-${face}-${scale}-${ground}.png`);
        }
    }
}
/* --------------------------------------------------- degenerate sizes ----
 * The sizes the widget actually passes through, as opposed to the ones a
 * user chooses. A widget is 0x0 between construction and first allocation,
 * and again while a monitor change is applied. The original arithmetic
 * scaled by (w - 20): negative below 20px, zero at 20, and NaN at 0, which
 * made the text render upside down, vanish, or throw. */
for (const [w, h] of [[470, 176], [231, 87], [100, 40], [40, 30], [20, 15],
                      [10, 10], [1, 1], [0, 0]]) {
    const surface = new Cairo.ImageSurface(Cairo.Format.ARGB32,
                                           Math.max(w, 1), Math.max(h, 1));
    const cr = new Cairo.Context(surface);
    let threw = null;
    const sizes = [];
    const baselines = [];
    const probe = new Proxy(cr, {get(target, key) {
        if (key === 'setFontSize')
            return v => { sizes.push(v); target.setFontSize(v); };
        if (key === 'moveTo')
            return (x, y) => { baselines.push(y); target.moveTo(x, y); };
        return typeof target[key] === 'function' ? target[key].bind(target) : target[key];
    }});
    try { drawUnavailable(probe, w, h); } catch (e) { threw = e.message; }
    ok(threw === null, `${w}x${h}: does not throw`, threw ?? '');
    ok(sizes.every(v => Number.isFinite(v) && v > 0),
       `${w}x${h}: every font size is finite and positive`, JSON.stringify(sizes));
    ok(baselines.every(y => y >= 0 && y <= h),
       `${w}x${h}: every baseline drawn is inside the surface`,
       `h=${h} baselines=${JSON.stringify(baselines)}`);
    try { cr.$dispose(); } catch { /* already gone */ }
}

print(`${checks} checks, ${failures} failure(s)`);
imports.system.exit(failures ? 1 : 0);
