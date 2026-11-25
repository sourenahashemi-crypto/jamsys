class Gesture {
    constructor(props = {}) {
        for (const k of Object.keys(props)) {
            if (!this.constructor.PROPS.includes(k))
                throw new Error(`${this.constructor.name}: no property '${k}'`);
        }
        Object.assign(this, props);
        this._handlers = {};
    }
    connect(sig, fn) {
        if (!this.constructor.SIGNALS.includes(sig))
            throw new Error(`${this.constructor.name}: no signal '${sig}'`);
        (this._handlers[sig] ??= []).push(fn);
        return 1;
    }
    disconnect() {}
    emit(sig, ...a) { (this._handlers[sig] ?? []).forEach(f => f(this, ...a)); }
}

export default {
    ActorAlign: {CENTER: 1},
    BinLayout: class {},
    EVENT_STOP: true,
    EVENT_PROPAGATE: false,
    // Real values from Clutter. SMOOTH is what Wayland actually delivers, so a
    // stub without it would let a handler that ignores smooth scroll pass.
    ScrollDirection: {UP: 0, DOWN: 1, LEFT: 2, RIGHT: 3, SMOOTH: 4},
    // Clutter 18 gestures. There is deliberately no DragAction here and no
    // get_click_count on events: both were removed in GNOME 50, and a stub that
    // kept them is what let an extension that could not load pass every test.
    // Property and signal names are the real ones (see real-api-test.js, which
    // checks this list against the installed typelib); anything else throws.
    ClickGesture: class extends Gesture {
        static PROPS = ['n_clicks_required', 'required_button', 'recognize_on_press'];
        static SIGNALS = ['recognize', 'end', 'cancel', 'may-recognize'];
    },
    PanGesture: class extends Gesture {
        static PROPS = ['required_button', 'begin_threshold', 'pan_axis',
                        'min_n_points', 'max_n_points'];
        static SIGNALS = ['recognize', 'end', 'cancel', 'may-recognize', 'pan-update'];
        get_accumulated_delta() { return this._delta ?? {x: 0, y: 0}; }
    },
};
