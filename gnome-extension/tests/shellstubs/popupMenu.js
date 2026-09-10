const ITEM_PARAMS = ['reactive', 'activate', 'hover', 'style_class', 'can_focus'];

export class PopupMenuItem {
    constructor(text, params) {
        for (const k of Object.keys(params ?? {}))
            if (!ITEM_PARAMS.includes(k))
                throw new Error(`PopupMenuItem: unrecognized parameter "${k}"`);
        this.label = {text, clutter_text: {}, add_style_class_name() {}};
        this.visible = true;
    }
    connect() { return 1; }
}
export class PopupSeparatorMenuItem {}

/* A chrome actor has no PanelMenu.Button to inherit a menu from, so the cluster
 * builds its own PopupMenu and registers it with a manager. Modelled here so the
 * contract test exercises the same calls the Shell will make. */

export class PopupMenu {
    constructor(sourceActor, alignment, side) {
        if (!sourceActor)
            throw new Error('PopupMenu: sourceActor is required');
        if (typeof alignment !== 'number')
            throw new Error('PopupMenu: alignment must be a number');
        if (typeof side !== 'number')
            throw new Error('PopupMenu: side must be an St.Side value');
        this.sourceActor = sourceActor;
        this.actor = {
            add_style_class_name() {},
            hide() { this.visible = false; },
            show() { this.visible = true; },
            visible: true,
        };
        this.items = [];
        this.isOpen = false;
    }
    addMenuItem(item) { this.items.push(item); }
    toggle() { this.isOpen = !this.isOpen; }
    open() { this.isOpen = true; }
    close() { this.isOpen = false; }
    destroy() { this.destroyed = true; }
    connect() { return 1; }
    disconnect() {}
}

export class PopupMenuManager {
    constructor(owner) {
        if (!owner)
            throw new Error('PopupMenuManager: an owner actor is required');
        this.menus = [];
    }
    addMenu(m) { this.menus.push(m); }
    removeMenu(m) { this.menus = this.menus.filter(x => x !== m); }
}
