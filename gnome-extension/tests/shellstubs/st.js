class Actor {
    constructor(p = {}) { Object.assign(this, p); this._children = []; }
    _init(p) { Object.assign(this, p ?? {}); }
    add_child(c) { this._children.push(c); }
    remove_child(c) { this._children = this._children.filter(x => x !== c); }
    set_size(w, h) { this._w = w; this._h = h; }
    get_size() { return [this._w ?? 420, this._h ?? 192]; }
    set_position(x, y) { this.x = x; this.y = y; }
    get_parent() { return null; }
    connect() { return 1; }
    disconnect() {}
    destroy() {}
    queue_repaint() { this.repaints = (this.repaints ?? 0) + 1; }
    add_style_class_name() {}
    remove_style_class_name() {}
}
export default {Widget: Actor, Bin: Actor, Label: Actor, DrawingArea: Actor,
                Button: Actor, Icon: Actor};
