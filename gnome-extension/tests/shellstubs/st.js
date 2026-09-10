class Actor {
    constructor(p = {}) { Object.assign(this, p); this._children = []; }
    _init(p) { Object.assign(this, p ?? {}); }
    add_child(c) { this._children.push(c); }
    remove_child(c) { this._children = this._children.filter(x => x !== c); }
    set_size(w, h) { this._w = w; this._h = h; }
    get_size() { return [this._w ?? 420, this._h ?? 192]; }
    set_position(x, y) { this.x = x; this.y = y; }
    get_position() { return [this.x ?? 0, this.y ?? 0]; }
    add_action(a) { (this._actions ??= []).push(a); }
    remove_action(a) { this._actions = (this._actions ?? []).filter(x => x !== a); }
    hide() { this.visible = false; }
    show() { this.visible = true; }
    get_parent() { return null; }
    // Handlers are recorded, not discarded: a test that cannot fire an event
    // cannot check what an event handler does, which is where the interesting
    // behaviour lives.
    connect(sig, fn) {
        (this._handlers ??= {})[sig] ??= [];
        this._handlers[sig].push(fn);
        return (this._nextId = (this._nextId ?? 0) + 1);
    }
    disconnect() {}
    emitEvent(sig, ...args) {
        let last;
        for (const fn of (this._handlers?.[sig] ?? []))
            last = fn(this, ...args);
        return last;
    }
    destroy() {}
    queue_repaint() { this.repaints = (this.repaints ?? 0) + 1; }
    add_style_class_name() {}
    remove_style_class_name() {}
}
export default {Widget: Actor, Bin: Actor, Label: Actor, DrawingArea: Actor,
                Button: Actor, Icon: Actor,
                Side: {TOP: 0, RIGHT: 1, BOTTOM: 2, LEFT: 3}};
