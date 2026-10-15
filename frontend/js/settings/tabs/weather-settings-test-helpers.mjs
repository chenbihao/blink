// Minimal event/element fixture: run the real editor and card handlers without Tauri or a DOM dependency.
export class TestElement {
    constructor(classes = "") {
        this.classes = new Set(classes.split(" ").filter(Boolean));
        this.classList = {
            contains: (name) => this.classes.has(name),
            add: (...names) => names.forEach((name) => this.classes.add(name)),
            remove: (...names) => names.forEach((name) => this.classes.delete(name)),
            toggle: (name, force = !this.classes.has(name)) => {
                if (force) this.classes.add(name);
                else this.classes.delete(name);
                return force;
            },
        };
        this.listeners = new Map();
        this.selectors = new Map();
        this.dataset = {};
        this.children = [];
        this.value = "";
        this.innerHTML = "";
        this.textContent = "";
        this.isConnected = true;
        this.disabled = false;
    }
    querySelector(selector) { return this.selectors.get(selector) ?? null; }
    querySelectorAll(selector) {
        const result = this.querySelector(selector);
        return result === null ? [] : Array.isArray(result) ? result : [result];
    }
    addEventListener(type, listener) {
        if (!this.listeners.has(type)) this.listeners.set(type, []);
        this.listeners.get(type).push(listener);
    }
    removeEventListener(type, listener) {
        this.listeners.set(type, (this.listeners.get(type) ?? []).filter((value) => value !== listener));
    }
    async fire(type, event = {}) {
        const supplied = {type, target: this, preventDefault() {}, stopPropagation() {}, ...event};
        for (const listener of this.listeners.get(type) ?? []) await listener(supplied);
    }
    dispatchEvent(event) { return this.fire(event.type, event); }
    matches(selector) {
        return selector.split(",").some((part) => this.classes.has(part.trim().replace(/^\./, "")));
    }
    closest() { return this; }
    contains() { return true; }
    setAttribute(name, value) { this[name] = value; }
    removeAttribute(name) { delete this[name]; }
    focus() {}
    blur() {}
    remove() { this.isConnected = false; }
    appendChild(child) { this.children.push(child); }
    insertBefore(child) { this.children.push(child); }
}

export function weatherCardFixture() {
    const card = new TestElement();
    const root = new TestElement("weather-settings");
    root.ownerDocument = {defaultView: {CustomEvent: class {
        constructor(type) { this.type = type; }
    }}};
    card.selectors.set(".weather-settings", root);
    for (const selector of ["[data-weather-editor]", "[data-weather-current]", "[data-weather-candidates]",
        "[data-weather-search-status]", "[data-weather-manual-status]", ".weather-query",
        ".weather-country-code", ".weather-admin", ".weather-resolver", ".weather-region",
        ".weather-latitude", ".weather-longitude", ".weather-manual-name", ".weather-edit-toggle",
        ".weather-manual-section"]) {
        root.selectors.set(selector, new TestElement(selector.startsWith(".") ? selector.slice(1) : ""));
    }
    root.querySelector(".weather-region").value = "unknown";
    root.querySelector(".weather-resolver").value = "auto";
    return {card, root};
}
