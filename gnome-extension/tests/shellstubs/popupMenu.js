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
