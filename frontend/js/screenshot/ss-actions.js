// Buttons, overflow items and keyboard entry points use the same disabled gate.
const actions = new Map();
export function registerScreenshotAction(id, run) { actions.set(id, run); }
export function runScreenshotAction(id) {
    const button = document.getElementById(`btn-${id}`);
    if (!button || button.disabled || button.hidden || button.classList.contains('hidden')) return false;
    const run = actions.get(id);
    if (!run) return false;
    run();
    return true;
}
