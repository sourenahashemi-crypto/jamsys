/* Pure formatting for the corner readout.
 *
 * Split out of extension.js deliberately: this is the only part with real logic, and
 * inside gnome-shell it cannot be unit-tested at all. Here it is a plain ES module that
 * `gjs` can import directly, so the label strings are covered by tests/format-test.js.
 */

export const pct = v => `${Math.round(v)}%`;
export const deg = v => `${Math.round(v)}°`;
export const watt = v => `${Math.abs(v).toFixed(1)}W`;

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
        parts.push(s.dgpu === 'suspended' ? 'GPU —' : `GPU ${pct(s.gpu_pct)}`);
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
    case 'Power':   return `${sub} ${watt(s.power_w)}`;
    case 'CPU':     return `${sub} ${pct(s.cpu_pct)} ${deg(s.cpu_temp_c)}`;
    case 'Memory':  return `${sub} ${pct(s.mem_pct)}`;
    case 'GPU':     return `${sub} ${pct(s.gpu_pct)}`;
    case 'Network': return `${sub} ${s.net_label}`;
    // These have no single number of their own, so the count is the measurement.
    case 'Services':
    case 'Devices':
    case 'Storage':
    case 'Thermals': {
        const n = s.open_alerts || 1;
        return `${sub} ${n}`;
    }
    default:        return sub || 'ATTENTION';
    }
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
