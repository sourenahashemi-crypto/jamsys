export default {
    ActorAlign: {CENTER: 1},
    BinLayout: class {},
    EVENT_STOP: true,
    EVENT_PROPAGATE: false,
    // Real values from Clutter. SMOOTH is what Wayland actually delivers, so a
    // stub without it would let a handler that ignores smooth scroll pass.
    ScrollDirection: {UP: 0, DOWN: 1, LEFT: 2, RIGHT: 3, SMOOTH: 4},
    // Drag-to-move. Records its handlers so a test can fire drag-end without a
    // pointer, which is the only way to check that a drop is persisted.
    DragAction: class {
        constructor() { this._handlers = {}; }
        connect(sig, fn) { (this._handlers[sig] ??= []).push(fn); return 1; }
        disconnect() {}
        emit(sig, ...a) { (this._handlers[sig] ?? []).forEach(f => f(this, ...a)); }
    },
};
