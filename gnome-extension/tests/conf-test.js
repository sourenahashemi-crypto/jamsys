#!/usr/bin/env gjs
/* The cluster window's settings and --quit bookkeeping.
 *
 *   gjs -m gnome-extension/tests/conf-test.js
 */
import {CONF_KEYS, confToSave, isClusterCmdline, sanitizeConf, scrollFactor}
    from '../jamsys@jamsys.org/conf.js';

let failures = 0;
const ok = (c, what, extra = '') => {
    print(`  ${c ? 'ok  ' : 'FAIL'} ${what}${c ? '' : '  ' + extra}`);
    if (!c) failures++;
};
const sorted = o => (o && typeof o === 'object' && !Array.isArray(o))
    ? Object.fromEntries(Object.entries(o).sort(([a], [b]) => a.localeCompare(b))) : o;
const same = (a, b) => JSON.stringify(sorted(a)) === JSON.stringify(sorted(b));
const L = {minScale: 0.6, maxScale: 4.0, minOpacity: 0.2};

print('a settings file is not trusted');
for (const bad of [null, 'text', 7, [1, 2], true])
    ok(same(sanitizeConf(bad, L), {}), `${JSON.stringify(bad)} loads as no settings, not a crash`);
ok(same(sanitizeConf({scale: 'big', opacity: null, bare: 'yes'}, L), {}),
   'wrong types are dropped rather than becoming NaN');
ok(sanitizeConf({scale: 99, opacity: 0}, L).scale === 4.0
   && sanitizeConf({scale: 99, opacity: 0}, L).opacity === 0.2, 'numbers are clamped');
ok(!('clickthrough' in sanitizeConf({clickthrough: true, sticky: true}, L)),
   'click-through is never restored from disk');
ok(sanitizeConf({clickthrough: true, sticky: true}, L).sticky === true,
   'while the settings beside it are');

print('\na command-line flag is for this run');
const disk = {scale: 1.3, bare: true, ontop: false, sticky: false, opacity: 0.92, decorated: false};
const now = {...disk, scale: 1.9, bare: false, ontop: true};
let out = confToSave(now, disk, new Set(['scale', 'bare', 'ontop']), new Set());
ok(same(out, disk), 'flags alone change nothing on disk', JSON.stringify(out));
out = confToSave({...now, opacity: 0.8}, disk, new Set(['scale', 'bare', 'ontop']),
                 new Set(['opacity']));
ok(out.opacity === 0.8 && out.scale === 1.3 && out.bare === true && out.ontop === false,
   'an unrelated change in the window does not smuggle the flags in', JSON.stringify(out));
out = confToSave({...now, scale: 2.2}, disk, new Set(['scale']), new Set(['scale']));
ok(out.scale === 2.2, 'but resizing after --scale saves the new size');
out = confToSave({...disk, scale: 0.7357620281346221}, {}, new Set(), new Set(['scale']));
ok(out.scale === 0.736, 'sizes are stored rounded', String(out.scale));
ok(!('clickthrough' in out) && Object.keys(out).every(k => CONF_KEYS.includes(k)),
   'and click-through is never written');
out = confToSave(now, {}, new Set(['scale']), new Set());
ok(!('scale' in out), 'a flag over a first run leaves the key unset, to take the default');

print('\n--quit only signals a cluster');
const z = (...a) => a.join('\0') + '\0';
ok(isClusterCmdline(z('gjs', '-m', '/home/u/.local/share/gnome-shell/extensions/jamsys@jamsys.org/standalone.js')),
   'the installed cluster');
ok(isClusterCmdline(z('/usr/bin/gjs-console', '-m', './standalone.js', '--on-top')),
   'gjs-console with arguments');
ok(!isClusterCmdline(z('bash', '-c', 'gjs -m /x/standalone.js')),
   'a shell that merely mentions the file is left alone');
ok(!isClusterCmdline(z('vim', '/x/standalone.js')), 'so is an editor with it open');
ok(!isClusterCmdline(z('gjs', '-m', '/x/not-standalone.js.bak')), 'and a different script');
ok(!isClusterCmdline('') && !isClusterCmdline(null), 'an unreadable cmdline matches nothing');

print('\none swipe is not twenty notches');
ok(Math.abs(scrollFactor(-1) - 1.08) < 1e-9 && Math.abs(scrollFactor(1) - 1 / 1.08) < 1e-9,
   'a wheel notch is 8% either way');
let k = 1;
for (let i = 0; i < 10; i++) k *= scrollFactor(-0.1);
ok(Math.abs(k - 1.08) < 1e-6, 'ten tenth-notch events make one notch', String(k));
ok(scrollFactor(-40) === scrollFactor(-1), 'a huge delta is one notch, not forty');
ok(scrollFactor(NaN) === 1, 'a nonsense delta changes nothing');

print(failures ? `\n${failures} FAILED` : '\nALL PASS');
imports.system.exit(failures ? 1 : 0);
