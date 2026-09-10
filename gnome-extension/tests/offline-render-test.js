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
print(`${checks} checks, ${failures} failure(s)`);
imports.system.exit(failures ? 1 : 0);
