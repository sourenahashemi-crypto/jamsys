/* Settings and process bookkeeping for the cluster window.
 *
 * Split out of standalone.js for the same reason format.js is split out of
 * extension.js: these are the parts with real logic, and inside a file that
 * opens a window on import they cannot be tested at all.
 */

export const CONF_KEYS = ['scale', 'opacity', 'decorated', 'bare', 'ontop', 'sticky'];

/** What a settings file may contribute, with anything malformed dropped.
 *
 * `JSON.parse` succeeds on `null`, on a string and on an array; the first of
 * those crashed startup ("saved is null") and a non-numeric scale became NaN
 * and then a zero-sized window. */
export function sanitizeConf(raw, limits) {
    const out = {};
    if (raw === null || typeof raw !== 'object' || Array.isArray(raw))
        return out;
    const num = (k, lo, hi) => {
        if (typeof raw[k] === 'number' && Number.isFinite(raw[k]))
            out[k] = Math.max(lo, Math.min(hi, raw[k]));
    };
    num('scale', limits.minScale, limits.maxScale);
    num('opacity', limits.minOpacity, 1.0);
    for (const k of ['decorated', 'bare', 'ontop', 'sticky'])
        if (typeof raw[k] === 'boolean') out[k] = raw[k];
    // `clickthrough` is deliberately not read. It makes the gadget unclickable
    // on Mutter, and the way out is `jamsys-cluster --quit` -- which is no way
    // out at all if the setting is waiting in the file for the next launch.
    return out;
}

/** What to write to disk.
 *
 * A value given on the command line is for this run only: `--scale 1.9` to try
 * a size, or `--housing` for a screenshot, used to be saved by the first
 * unrelated change and became the permanent setting. Such a key keeps its
 * on-disk value unless the user then changed it in the window.
 */
export function confToSave(current, onDisk, cliKeys, changedKeys) {
    const out = {};
    for (const k of CONF_KEYS) {
        const fromCli = cliKeys.has(k) && !changedKeys.has(k);
        const v = fromCli ? onDisk[k] : current[k];
        if (v !== undefined)
            out[k] = k === 'scale' || k === 'opacity' ? Math.round(v * 1000) / 1000 : v;
    }
    return out;
}

/** Is this `/proc/<pid>/cmdline` a running cluster window?
 *
 * The check was `includes('standalone.js')`, which also matched any shell or
 * editor whose command line merely mentioned the file -- and `--quit` then sent
 * that process a SIGTERM. Compare the argv itself: gjs running a script whose
 * path ends in /standalone.js.
 */
export function isClusterCmdline(cmdline) {
    const argv = String(cmdline ?? '').split('\0').filter(a => a.length);
    if (argv.length < 2)
        return false;
    const prog = argv[0].split('/').pop();
    if (!/^gjs(-console)?$/.test(prog))
        return false;
    return argv.slice(1).some(a => a === 'standalone.js' || a.endsWith('/standalone.js'));
}

/** How much one scroll event resizes: a notch is 8%, a fraction of a notch is
 *  that fraction. Touchpads send dozens of small-delta events per swipe. */
export function scrollFactor(dy) {
    if (!Number.isFinite(dy))
        return 1;
    return Math.pow(1.08, -Math.max(-1, Math.min(1, dy)));
}
