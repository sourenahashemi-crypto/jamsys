/* GObject.registerClass does more than hand the class back: a registered class is
 * constructed through GObject, which invokes `_init` rather than the JS constructor.
 * A stub that just returns the class leaves `_init` uncalled, so every field the
 * widget sets up is missing and the failure looks like a bug in the extension.
 * Mirror the real behaviour instead. */
export default {
    registerClass(cls) {
        return class extends cls {
            constructor(...args) {
                super();
                if (typeof this._init === 'function')
                    this._init(...args);
            }
        };
    },
};
