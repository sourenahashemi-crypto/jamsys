/* Strict stand-ins for the gnome-shell APIs the extension touches.
 *
 * These deliberately mirror GNOME 50's `Params.parse` behaviour: an unrecognised key
 * is an error, not something quietly ignored. That is exactly how
 * `addChrome({affectsInputRegion: true})` slipped through review, loaded fine under a
 * permissive stub, and then failed only inside the live Shell — where the extension
 * cannot be reloaded without a logout, so the round trip to find it is a whole
 * session. Encoding the real contract here makes that a test failure in a second.
 */

function strictParams(name, params, allowed) {
    for (const k of Object.keys(params ?? {})) {
        if (!allowed.includes(k))
            throw new Error(`${name}: unrecognized parameter "${k}"`);
    }
}

/** GNOME 50 LayoutManager.addChrome accepts exactly these. */
export const ADD_CHROME_PARAMS = ['affectsStruts', 'trackFullscreen'];
/** ...and trackChrome additionally accepts affectsInputRegion. */
export const TRACK_CHROME_PARAMS = ['affectsStruts', 'affectsInputRegion', 'trackFullscreen'];

export const layoutManager = {
    chrome: [],
    primaryMonitor: {x: 0, y: 0, width: 1920, height: 1080},
    addChrome(actor, params) {
        strictParams('addChrome', params, ADD_CHROME_PARAMS);
        this.chrome.push(actor);
    },
    trackChrome(actor, params) {
        strictParams('trackChrome', params, TRACK_CHROME_PARAMS);
    },
    removeChrome(actor) {
        this.chrome = this.chrome.filter(a => a !== actor);
    },
    connect() { return 1; },
    disconnect() {},
};

export const panel = {
    height: 32,
    statusArea: {},
    addToStatusArea(role, indicator, position, box) {
        if (!['left', 'center', 'right'].includes(box ?? 'right'))
            throw new Error(`addToStatusArea: bad box "${box}"`);
        this.statusArea[role] = indicator;
        return indicator;
    },
};

export function notify() {}
