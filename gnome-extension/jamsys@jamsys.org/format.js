/* Pure formatting for the corner readout.
 *
 * Split out of extension.js deliberately: this is the only part with real logic, and
 * inside gnome-shell it cannot be unit-tested at all. Here it is a plain ES module that
 * `gjs` can import directly, so the label strings are covered by tests/format-test.js.
 */

/* Each prints a dash for anything that is not a number. The daemon can send a
 * state with fields missing, and "CPU NaN%" in the top bar is worse than "CPU —". */
const fin = Number.isFinite;
export const pct = v => fin(v) ? `${Math.round(v)}%` : '—';
export const deg = v => fin(v) ? `${Math.round(v)}°` : '—';
export const watt = v => fin(v) ? `${Math.abs(v).toFixed(1)}W` : '—';
/** Signed form: a negative reading is charge going in, not power coming out. */
export const wattSigned = (v, onBattery) =>
    onBattery || v >= 0 ? watt(v) : `+${watt(v)}`;

/** The line shown while nothing is wrong. `show` decides which readings appear. */
export function normalLine(s, show, minimal) {
    const parts = [];
    if (show['show-cpu'])
        parts.push(show['show-temp'] && s.cpu_temp_c > 0
            ? `CPU ${pct(s.cpu_pct)} · ${deg(s.cpu_temp_c)}`
            : `CPU ${pct(s.cpu_pct)}`);
    if (show['show-ram'])
        parts.push(`RAM ${pct(s.mem_pct)}`);
    if (show['show-gpu'])
        // Measured only while the card is awake; see readings() in gauges.js.
        parts.push(s.dgpu === 'active' ? `GPU ${pct(s.gpu_pct)}` : 'GPU —');
    if (show['show-power'] && s.has_battery)
        parts.push(s.on_battery ? watt(s.power_w) : `AC ${pct(s.battery_pct)}`);
    if (show['show-net'])
        parts.push(s.net_ok ? 'NET ✓' : 'NET ✗');
    return minimal ? parts.join('\n') : parts.join(' | ');
}

/** A few words naming the abnormal subsystem and its measurement. */
export function shortAlert(s) {
    const sub = (s.alert_subsystem || '').toUpperCase();
    switch (s.alert_subsystem) {
    case 'Power':   return `${sub} ${wattSigned(s.power_w, s.on_battery)}`;
    case 'CPU':     return s.cpu_temp_c > 0
        ? `${sub} ${pct(s.cpu_pct)} ${deg(s.cpu_temp_c)}`
        : `${sub} ${pct(s.cpu_pct)}`;   // no sensor: "0°" would be a made-up reading
    case 'Memory':  return `${sub} ${pct(s.mem_pct)}`;
    case 'GPU':     return `${sub} ${pct(s.gpu_pct)}`;
    case 'Network': return `${sub} ${s.net_label}`;
    // These have no single number of their own, so the count is the measurement.
    case 'Services':
    case 'Security':
    case 'Devices':
    case 'Storage':
    case 'Thermals': {
        const n = s.open_alerts || 1;
        return `${sub} ${n}`;
    }
    default:        return sub || 'ATTENTION';
    }
}

/** One sentence saying what the gadget shows, for a screen reader.
 *
 * The cluster is a drawing: without this, an assistive technology found an
 * unnamed image and nothing else. Abnormal state comes first and in full --
 * the drawn alert pill is cut to a few dozen characters, and this is also
 * where a sighted user can read the rest (it doubles as the tooltip).
 */
export function describe(s) {
    if (!s)
        return 'JamSys: waiting for the monitoring service';
    const parts = [];
    if ((s.health === 'attention' || s.health === 'critical') && s.alert_title) {
        parts.push(`${s.health === 'critical' ? 'Critical' : 'Attention'}: ${s.alert_title}`);
        if (s.alert_detail)
            parts.push(s.alert_detail);
    } else {
        parts.push('Everything is normal');
    }
    const cpu = s.cpu_temp_c > 0 ? `CPU ${pct(s.cpu_pct)} at ${deg(s.cpu_temp_c)}` : `CPU ${pct(s.cpu_pct)}`;
    parts.push(`${cpu}, memory ${pct(s.mem_pct)}`);
    if (s.has_battery && Number.isFinite(s.battery_pct))
        parts.push(`battery ${pct(s.battery_pct)}${s.on_battery ? '' : ', charging or on mains'}`);
    parts.push(s.net_ok === false ? 'network down' : 'network up');
    return `JamSys. ${parts.join('. ')}.`;
}

/** The whole label: abnormal state replaces the readings entirely. */
export function labelFor(s, show, minimal) {
    const abnormal = s.health === 'attention' || s.health === 'critical';
    if (abnormal && s.alert_title)
        return (s.health === 'critical' ? '✕ ' : '⚠ ') + shortAlert(s);
    return normalLine(s, show, minimal);
}

export function styleFor(health) {
    switch (health) {
    case 'critical':  return 'jamsys-critical';
    case 'attention': return 'jamsys-attention';
    case 'healthy':   return 'jamsys-healthy';
    default:          return 'jamsys-unknown';
    }
}
