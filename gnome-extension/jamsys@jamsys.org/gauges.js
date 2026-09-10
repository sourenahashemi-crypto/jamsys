/* JamSys instrument cluster — pure Cairo drawing.
 *
 * No Shell imports here on purpose. Inside gnome-shell this is called from an
 * `St.DrawingArea` repaint handler; outside it, `tests/render-cluster.js` calls the
 * identical code against an `ImageSurface` and writes a PNG. That is the only way to
 * actually *look* at a design without being able to load the extension, and it means
 * the drawing is reviewable and regression-testable rather than taken on trust.
 *
 * The visual language is a car instrument cluster: a swept tachometer dial with a
 * redline, machined bezels, a tapered needle with a counterweight, and a row of
 * telltales that stay dark until they have something to say.
 */

const TAU = Math.PI * 2;

/* Sweep of every round gauge: 135° through 405°, the classic 270° automotive dial. */
export const A0 = Math.PI * 0.75;
export const SWEEP = Math.PI * 1.5;

/* ------------------------------------------------------------------ palette */

export const C = {
    // Instrument housing.
    panel0:     [0.055, 0.067, 0.078],
    panel1:     [0.020, 0.027, 0.035],
    panelEdge:  [0.28, 0.33, 0.37],
    bezelHi:    [0.34, 0.38, 0.42],
    bezelLo:    [0.055, 0.075, 0.090],
    face0:      [0.075, 0.090, 0.105],
    face1:      [0.030, 0.038, 0.047],

    // Markings.
    tickMinor:  [0.42, 0.48, 0.53],
    tickMajor:  [0.80, 0.85, 0.89],
    numeral:    [0.62, 0.68, 0.73],
    label:      [0.48, 0.55, 0.60],

    // Readouts. Warm amber is the cluster's voice; cyan is for secondary data.
    readout:    [1.00, 0.72, 0.33],
    readoutDim: [0.55, 0.42, 0.22],
    secondary:  [0.42, 0.78, 0.92],

    // Zones, shared with the rest of the application's status colours.
    green:      [0.18, 0.76, 0.49],
    amber:      [0.90, 0.65, 0.04],
    red:        [0.88, 0.11, 0.14],

    needle:     [1.00, 0.38, 0.20],
    needleGlow: [1.00, 0.30, 0.12],
    hub:        [0.72, 0.76, 0.80],
};

function rgba(cr, c, a = 1) {
    cr.setSourceRGBA(c[0], c[1], c[2], a);
}

/* ------------------------------------------------------------------ helpers */

export function roundRect(cr, x, y, w, h, r) {
    r = Math.min(r, w / 2, h / 2);
    cr.newSubPath();
    cr.arc(x + w - r, y + r, r, -Math.PI / 2, 0);
    cr.arc(x + w - r, y + h - r, r, 0, Math.PI / 2);
    cr.arc(x + r, y + h - r, r, Math.PI / 2, Math.PI);
    cr.arc(x + r, y + r, r, Math.PI, Math.PI * 1.5);
    cr.closePath();
}

/** Clamp a value into 0..1 and survive NaN, which sensors do produce. */
export function norm(v, min, max) {
    if (!Number.isFinite(v)) return 0;
    if (max <= min) return 0;
    return Math.max(0, Math.min(1, (v - min) / (max - min)));
}

function text(cr, s, x, y, size, color, {align = 'center', bold = false, alpha = 1} = {}) {
    cr.selectFontFace('Ubuntu Sans Mono', 0, bold ? 1 : 0);
    cr.setFontSize(size);
    const e = cr.textExtents(s);
    let tx = x;
    if (align === 'center') tx = x - (e.width / 2 + e.xBearing);
    else if (align === 'right') tx = x - (e.width + e.xBearing);
    rgba(cr, color, alpha);
    cr.moveTo(tx, y);
    cr.showText(s);
    cr.newPath();
}

/* ------------------------------------------------------------------- panel */

/** The housing: a dark slab with a machined edge and a soft interior vignette. */
export function drawPanel(cr, w, h, {radius = 14, opacity = 0.90} = {}) {
    const g = new imports.cairo.LinearGradient(0, 0, 0, h);
    g.addColorStopRGBA(0, ...C.panel0, opacity);
    g.addColorStopRGBA(1, ...C.panel1, opacity);
    roundRect(cr, 0.5, 0.5, w - 1, h - 1, radius);
    cr.setSource(g);
    cr.fill();

    // A single hairline edge reads as machined metal without any skeuomorphic noise.
    roundRect(cr, 0.5, 0.5, w - 1, h - 1, radius);
    rgba(cr, C.panelEdge, 0.55 * opacity);
    cr.setLineWidth(1);
    cr.stroke();
}

/* ------------------------------------------------------------------ gauges */

/**
 * One round instrument.
 *
 * `zones` paint the coloured band just inside the bezel — green through to the
 * redline — exactly as a tachometer does, so the needle's position against colour is
 * readable before any number is.
 */
export function drawGauge(cr, cx, cy, r, o) {
    const value = Number.isFinite(o.value) ? Math.max(0, Math.min(1, o.value)) : 0;
    const dim = o.dim === true;
    const a = dim ? 0.38 : 1;

    // Bezel: a ring lit from the top left.
    const bez = new imports.cairo.LinearGradient(cx - r, cy - r, cx + r, cy + r);
    bez.addColorStopRGBA(0, ...C.bezelHi, a);
    bez.addColorStopRGBA(1, ...C.bezelLo, a);
    cr.arc(cx, cy, r, 0, TAU);
    cr.setSource(bez);
    cr.fill();

    // Dial face, slightly domed.
    const face = new imports.cairo.RadialGradient(cx, cy - r * 0.35, r * 0.1, cx, cy, r);
    face.addColorStopRGBA(0, ...C.face0, a);
    face.addColorStopRGBA(1, ...C.face1, a);
    cr.arc(cx, cy, r - 2.5, 0, TAU);
    cr.setSource(face);
    cr.fill();

    // Coloured zone band.
    const bandR = r - 7;
    const bandW = 3.5;
    cr.setLineWidth(bandW);
    cr.setLineCap(0);
    for (const z of o.zones || []) {
        rgba(cr, z.color, (z.alpha ?? 0.85) * a);
        cr.arc(cx, cy, bandR, A0 + SWEEP * z.from, A0 + SWEEP * z.to);
        cr.stroke();
    }

    // Ticks. Majors carry a numeral on the large dial; minors are just cadence.
    // Numerals are only drawn where there is genuinely room — a 44px auxiliary gauge
    // with numbers on it is unreadable, and real clusters leave them off too.
    const majors = o.majors ?? 5;
    const minorsPer = o.minorsPer ?? 4;
    const total = majors * minorsPer;
    const tickOuter = bandR - bandW / 2 - 2;
    const numeralSize = Math.max(7.5, r * 0.135);
    const showNumerals = o.numerals !== false && r >= 55;

    for (let i = 0; i <= total; i++) {
        const t = i / total;
        const ang = A0 + SWEEP * t;
        const isMajor = i % minorsPer === 0;
        const len = isMajor ? r * 0.12 : r * 0.070;
        const r0 = tickOuter - len;
        cr.setLineWidth(isMajor ? 1.9 : 1);
        rgba(cr, isMajor ? C.tickMajor : C.tickMinor, (isMajor ? 0.95 : 0.55) * a);
        cr.moveTo(cx + Math.cos(ang) * r0, cy + Math.sin(ang) * r0);
        cr.lineTo(cx + Math.cos(ang) * tickOuter, cy + Math.sin(ang) * tickOuter);
        cr.stroke();

        if (isMajor && showNumerals) {
            const nr = r0 - numeralSize * 0.55;
            const n = Math.round((o.min ?? 0) + ((o.max ?? 100) - (o.min ?? 0)) * t);
            text(cr, String(n), cx + Math.cos(ang) * nr,
                 cy + Math.sin(ang) * nr + numeralSize * 0.36, numeralSize, C.numeral,
                 {alpha: 0.9 * a});
        }
    }

    // Caption is dial furniture: drawn under the needle, and a needle sweeping across
    // it is authentic rather than a defect.
    if (o.caption)
        text(cr, o.caption, cx, cy - r * 0.30, Math.max(7.5, r * 0.150), C.label,
             {bold: true, alpha: 0.85 * a});

    drawNeedle(cr, cx, cy, r, A0 + SWEEP * value, {dim, color: o.needleColor});

    // The value is the one thing that must always be readable, so it sits in a
    // recessed digital window drawn *over* the needle — the inset LCD every modern
    // instrument cluster has, and the reason the needle never obscures the number.
    const readSize = Math.max(11, r * 0.34);
    if (o.readout) {
        const by = cy + r * 0.37;
        cr.selectFontFace('Ubuntu Sans Mono', 0, 1);
        cr.setFontSize(readSize);
        const wb = cr.textExtents(o.readout).width;
        cr.setFontSize(readSize * 0.52);
        const wu = o.unit ? cr.textExtents(o.unit).width + readSize * 0.14 : 0;
        const pw = wb + wu + readSize * 0.35;
        const ph = readSize * 1.28;
        roundRect(cr, cx - pw / 2, by - ph * 0.78, pw, ph, 3);
        rgba(cr, C.panel1, 0.88 * (dim ? 0.7 : 1));
        cr.fill();
        roundRect(cr, cx - pw / 2 + 0.5, by - ph * 0.78 + 0.5, pw - 1, ph - 1, 3);
        rgba(cr, C.bezelHi, 0.30 * a);
        cr.setLineWidth(1);
        cr.stroke();
        textPair(cr, cx, by, o.readout, o.unit || '', readSize,
                 o.readoutColor || C.readout, C.readoutDim,
                 // A dimmed dial still has to be legible; only the furniture fades.
                 dim ? 0.85 : a);
    }
    if (o.sub)
        text(cr, o.sub, cx, cy + r * 0.74, Math.max(7.5, r * 0.150),
             o.subColor || C.secondary, {alpha: 0.9 * a});
}

/** Right-align a value+unit group so its right edge lands on `rx`. */
function textPairRight(cr, rx, y, big, small, size, bigColor, smallColor, alpha = 1) {
    cr.selectFontFace('Ubuntu Sans Mono', 0, 1);
    cr.setFontSize(size);
    const wb = cr.textExtents(big).width;
    cr.setFontSize(size * 0.52);
    const ws = small ? cr.textExtents(small).width + size * 0.14 : 0;
    textPair(cr, rx - (wb + ws) / 2, y, big, small, size, bigColor, smallColor, alpha);
}

/** A big value with a small unit beside it, centred as one group. */
function textPair(cr, cx, y, big, small, size, bigColor, smallColor, alpha = 1) {
    cr.selectFontFace('Ubuntu Sans Mono', 0, 1);
    cr.setFontSize(size);
    const eb = cr.textExtents(big);
    let sw = 0, es = null;
    if (small) {
        cr.setFontSize(size * 0.52);
        es = cr.textExtents(small);
        sw = es.width + size * 0.14;
    }
    const startX = cx - (eb.width + sw) / 2;
    cr.setFontSize(size);
    rgba(cr, bigColor, alpha);
    cr.moveTo(startX - eb.xBearing, y);
    cr.showText(big);
    cr.newPath();
    if (small) {
        cr.setFontSize(size * 0.52);
        rgba(cr, smallColor, alpha);
        cr.moveTo(startX + eb.width + size * 0.14 - es.xBearing, y);
        cr.showText(small);
        cr.newPath();
    }
}

/** Tapered needle with a counterweight tail and a machined hub. */
function drawNeedle(cr, cx, cy, r, ang, {dim = false, color} = {}) {
    const a = dim ? 0.4 : 1;
    const col = color || C.needle;
    const tip = r - 10;
    const tail = r * 0.20;
    const halfW = Math.max(1.6, r * 0.055);
    const ca = Math.cos(ang), sa = Math.sin(ang);
    const px = -sa, py = ca;   // perpendicular

    // A faint bloom under the needle, the way a lit pointer reads at night.
    if (!dim) {
        cr.setLineWidth(halfW * 3.2);
        cr.setLineCap(1);
        rgba(cr, C.needleGlow, 0.13);
        cr.moveTo(cx, cy);
        cr.lineTo(cx + ca * tip, cy + sa * tip);
        cr.stroke();
    }

    cr.moveTo(cx + ca * tip, cy + sa * tip);
    cr.lineTo(cx + px * halfW - ca * tail * 0.2, cy + py * halfW - sa * tail * 0.2);
    cr.lineTo(cx - ca * tail + px * halfW * 0.75, cy - sa * tail + py * halfW * 0.75);
    cr.lineTo(cx - ca * tail - px * halfW * 0.75, cy - sa * tail - py * halfW * 0.75);
    cr.lineTo(cx - px * halfW - ca * tail * 0.2, cy - py * halfW - sa * tail * 0.2);
    cr.closePath();
    rgba(cr, col, a);
    cr.fill();

    const hubR = Math.max(2.4, r * 0.085);
    const hg = new imports.cairo.RadialGradient(cx - hubR * 0.4, cy - hubR * 0.4, 0, cx, cy, hubR);
    hg.addColorStopRGBA(0, ...C.hub, a);
    hg.addColorStopRGBA(1, ...C.bezelLo, a);
    cr.arc(cx, cy, hubR, 0, TAU);
    cr.setSource(hg);
    cr.fill();
}

/* -------------------------------------------------------------- linear bar */

/** Horizontal scale, used for power draw. Segmented, like a shift-light strip. */
export function drawBar(cr, x, y, w, h, o) {
    const value = Number.isFinite(o.value) ? Math.max(0, Math.min(1, o.value)) : 0;
    roundRect(cr, x, y, w, h, h / 2);
    rgba(cr, C.face1, 0.95);
    cr.fill();
    roundRect(cr, x + 0.5, y + 0.5, w - 1, h - 1, h / 2);
    rgba(cr, C.bezelHi, 0.35);
    cr.setLineWidth(1);
    cr.stroke();

    // Segments rather than a smooth fill: a continuous bar invites reading precision
    // that the underlying measurement does not have.
    const segs = o.segments ?? 22;
    const gap = 1.6;
    const sw = (w - 4 - gap * (segs - 1)) / segs;
    const lit = Math.round(value * segs);
    for (let i = 0; i < segs; i++) {
        const t = (i + 0.5) / segs;
        let col = C.green;
        if (o.zones) {
            for (const z of o.zones)
                if (t >= z.from && t <= z.to) col = z.color;
        }
        const on = i < lit;
        roundRect(cr, x + 2 + i * (sw + gap), y + 2, sw, h - 4, 1.2);
        rgba(cr, col, on ? 0.95 : 0.10);
        cr.fill();
    }
}

/* --------------------------------------------------------------- telltales */

/**
 * A dashboard warning lamp. Dark until it has something to say — which is the whole
 * point of them, and why they are the only part of the cluster that ever glows.
 */
export function drawTelltale(cr, x, y, w, h, label, state) {
    const col = state === 'critical' ? C.red
              : state === 'warn'     ? C.amber
              : state === 'info'     ? C.secondary
              : C.tickMinor;
    const lit = state && state !== 'off';

    if (lit) {
        roundRect(cr, x - 2, y - 2, w + 4, h + 4, 5);
        rgba(cr, col, 0.16);
        cr.fill();
    }
    roundRect(cr, x, y, w, h, 3.5);
    rgba(cr, col, lit ? 0.22 : 0.07);
    cr.fill();
    roundRect(cr, x + 0.5, y + 0.5, w - 1, h - 1, 3.5);
    rgba(cr, col, lit ? 0.95 : 0.28);
    cr.setLineWidth(1);
    cr.stroke();
    text(cr, label, x + w / 2, y + h / 2 + 3.4, 8.5, col, {bold: true, alpha: lit ? 1 : 0.45});
}

/** Warning triangle with an exclamation, drawn rather than typeset. */
function markWarn(cr, x, y, sz, col, a = 1) {
    const h = sz * 0.88;
    cr.moveTo(x, y - h / 2);
    cr.lineTo(x + sz / 2, y + h / 2);
    cr.lineTo(x - sz / 2, y + h / 2);
    cr.closePath();
    rgba(cr, col, 0.9 * a);
    cr.setLineWidth(1.4);
    cr.stroke();
    cr.setLineWidth(1.4);
    cr.setLineCap(1);
    cr.moveTo(x, y - h * 0.14);
    cr.lineTo(x, y + h * 0.16);
    cr.stroke();
    cr.arc(x, y + h * 0.33, 0.8, 0, TAU);
    cr.fill();
}

/** A cross, drawn rather than typeset. */
function markCross(cr, x, y, sz, col, a = 1) {
    const d = sz * 0.40;
    rgba(cr, col, 0.95 * a);
    cr.setLineWidth(1.8);
    cr.setLineCap(1);
    cr.moveTo(x - d, y - d); cr.lineTo(x + d, y + d); cr.stroke();
    cr.moveTo(x + d, y - d); cr.lineTo(x - d, y + d); cr.stroke();
}

/* ----------------------------------------------------------------- cluster */

/** Natural size at scale 1. The widget scales this uniformly.
 *  Wide and low, the proportions of an actual instrument binnacle. */
export const CLUSTER_W = 420;
export const CLUSTER_H = 192;

/** Map a JamSys state object onto instrument readings. */
export function readings(s) {
    const cpuTemp = Number.isFinite(s.cpu_temp_c) ? s.cpu_temp_c : 0;
    const dgpuAsleep = s.dgpu === 'suspended';
    return {
        cpu:  {v: norm(s.cpu_pct, 0, 100), text: `${Math.round(s.cpu_pct)}`,
               sub: cpuTemp > 0 ? `${Math.round(cpuTemp)}\u00b0C` : '',
               // Temperature earns a colour of its own: a cluster that shows 97 °C
               // in the same calm cyan as 45 °C is throwing away the reading.
               subColor: cpuTemp >= 90 ? C.red : cpuTemp >= 75 ? C.amber : C.secondary},
        ram:  {v: norm(s.mem_pct, 0, 100), text: `${Math.round(s.mem_pct)}`},
        gpu:  {v: dgpuAsleep ? 0 : norm(s.gpu_pct, 0, 100),
               text: dgpuAsleep ? '\u2014' : `${Math.round(s.gpu_pct)}`,
               dim: dgpuAsleep},
        // 60 W full scale covers this class of laptop under load without leaving the
        // needle in the first eighth of the dial during ordinary use.
        // With no battery there is no whole-system power figure at all unless the
        // privileged RAPL helper is installed, so say so rather than drawing a zero.
        power: {v: norm(Math.abs(s.power_w), 0, 60),
                text: s.has_battery ? `${Math.abs(s.power_w).toFixed(1)}` : '\u2014',
                known: s.has_battery,
                charging: !s.on_battery && s.has_battery},
        battery: {v: norm(s.battery_pct, 0, 100), pct: Math.round(s.battery_pct)},
    };
}

const ZONES_LOAD = [
    {from: 0.00, to: 0.70, color: C.green, alpha: 0.50},
    {from: 0.70, to: 0.90, color: C.amber, alpha: 0.70},
    {from: 0.90, to: 1.00, color: C.red,   alpha: 0.95},
];

/** Compose the whole instrument cluster into a `w` x `h` context. */
export function drawCluster(cr, w, h, s, {opacity = 0.92} = {}) {
    const k = Math.min(w / CLUSTER_W, h / CLUSTER_H);
    drawPanel(cr, w, h, {opacity});
    cr.save();
    cr.scale(k, k);

    const r = readings(s);
    const critical = s.health === 'critical';
    const attention = s.health === 'attention';

    // --- header: the name, or the one thing that is wrong -------------------
    if ((critical || attention) && s.alert_title) {
        const col = critical ? C.red : C.amber;
        roundRect(cr, 12, 7, CLUSTER_W - 24, 18, 4);
        rgba(cr, col, 0.13);
        cr.fill();
        roundRect(cr, 12.5, 7.5, CLUSTER_W - 25, 17, 4);
        rgba(cr, col, 0.5);
        cr.setLineWidth(1);
        cr.stroke();
        const msg = s.alert_title.length > 46 ? s.alert_title.slice(0, 45) + '...' : s.alert_title;
        // Centre the mark and the text as one group.
        cr.selectFontFace('Ubuntu Sans Mono', 0, 1);
        cr.setFontSize(9.5);
        const tw = cr.textExtents(msg.toUpperCase()).width;
        const mx = CLUSTER_W / 2 - (tw + 16) / 2 + 5;
        if (critical) markCross(cr, mx, 16, 10, col);
        else markWarn(cr, mx, 16, 11, col);
        text(cr, msg.toUpperCase(), mx + 11, 20, 9.5, col, {align: 'left', bold: true});
    } else {
        text(cr, 'J A M S Y S', CLUSTER_W / 2, 20, 8.5, C.label, {bold: true, alpha: 0.45});
    }

    // --- dials --------------------------------------------------------------
    // Central tachometer: CPU load, redlined like an engine.
    drawGauge(cr, 210, 100, 62, {
        value: r.cpu.v, min: 0, max: 100, majors: 4, minorsPer: 5,
        zones: ZONES_LOAD, caption: 'CPU', readout: r.cpu.text, unit: '%',
        sub: r.cpu.sub, subColor: r.cpu.subColor,
        needleColor: critical ? C.red : C.needle,
    });
    drawGauge(cr, 76, 104, 46, {
        value: r.ram.v, min: 0, max: 100, majors: 4, minorsPer: 5,
        zones: ZONES_LOAD, caption: 'RAM', readout: r.ram.text, unit: '%',
    });
    drawGauge(cr, 344, 104, 46, {
        value: r.gpu.v, min: 0, max: 100, majors: 4, minorsPer: 5,
        zones: ZONES_LOAD, caption: 'GPU', readout: r.gpu.text,
        unit: r.gpu.dim ? '' : '%', dim: r.gpu.dim,
    });

    // --- bottom strip: power, charge, telltales ------------------------------
    // Laid out on explicit x stops so the fields cannot run into one another as
    // values change width.
    const y = 162;
    const X = {pwrLabel: 14, bar: 40, barW: 110, pwrValue: 196,
               batLabel: 206, batBar: 232, batBarW: 40, batPct: 278, lamps: 316};

    text(cr, r.power.charging ? 'CHG' : 'PWR', X.pwrLabel, y + 11, 8.5, C.label,
         {align: 'left', bold: true, alpha: r.power.known ? 1 : 0.5});
    drawBar(cr, X.bar, y + 1, X.barW, 13, {
        segments: 15,
        value: r.power.known ? r.power.v : 0,
        zones: r.power.charging
            ? [{from: 0, to: 1, color: C.secondary}]
            : [{from: 0.00, to: 0.55, color: C.green},
               {from: 0.55, to: 0.80, color: C.amber},
               {from: 0.80, to: 1.00, color: C.red}],
    });
    textPairRight(cr, X.pwrValue, y + 13, r.power.text, r.power.known ? 'W' : '',
                  15, r.power.known ? C.readout : C.readoutDim, C.readoutDim);
    if (!r.power.known)
        text(cr, 'no battery sensor', X.batLabel, y + 11, 7.5,
             C.label, {align: 'left', alpha: 0.55});

    if (s.has_battery) {
        const low = r.battery.v < 0.15;
        const bcol = low ? C.red : r.battery.v < 0.30 ? C.amber : C.secondary;
        text(cr, 'BAT', X.batLabel, y + 11, 8.5, C.label, {align: 'left', bold: true});
        roundRect(cr, X.batBar, y + 3, X.batBarW, 9, 2);
        rgba(cr, C.face1, 0.95);
        cr.fill();
        roundRect(cr, X.batBar, y + 3, Math.max(2, X.batBarW * r.battery.v), 9, 2);
        rgba(cr, bcol, 0.9);
        cr.fill();
        roundRect(cr, X.batBar + 0.5, y + 3.5, X.batBarW - 1, 8, 2);
        rgba(cr, C.bezelHi, 0.35);
        cr.setLineWidth(1);
        cr.stroke();
        text(cr, `${r.battery.pct}%`, X.batPct, y + 11, 9.5, bcol,
             {align: 'left', bold: true});
    }

    // Telltales: dark until they have something to say.
    const lamps = [
        ['NET', s.net_ok ? 'off' : 'critical'],
        ['SVC', s.alert_subsystem === 'Services' ? (critical ? 'critical' : 'warn') : 'off'],
        ['SYS', critical ? 'critical' : attention ? 'warn' : 'off'],
    ];
    lamps.forEach(([label, state], i) => {
        drawTelltale(cr, X.lamps + i * 30, y + 1, 26, 13, label, state);
    });

    cr.restore();
}
