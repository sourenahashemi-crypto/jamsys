export class Extension {
    constructor(settings) { this.uuid = 'jamsys@jamsys.org'; this._s = settings; }
    getSettings() { return this._s; }
}
