export class Button {
    // The menu records what is added to it. Discarding the items made the stub
    // unable to answer the only interesting question about a menu: what is in it.
    _init() {
        this.menu = {
            items: [],
            addMenuItem(i) { this.items.push(i); },
            open() { this.isOpen = true; },
            close() { this.isOpen = false; },
            toggle() { this.isOpen = !this.isOpen; },
            connect: () => 1,
        };
    }
    add_child() {}
    destroy() {}
    get container() { return {get_parent: () => null}; }
}
