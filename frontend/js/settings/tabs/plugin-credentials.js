import {invoke} from "../../shared/tauri.js";
import {t} from "../../i18n/index.js";

/** Group summaries show presence only; they do not claim that authentication succeeded. */
export function refreshCredentialGroups(card, schema, settings) {
    const groups = new Map();
    for (const field of schema) {
        if (!field.group) continue;
        const title = typeof field.group === "string" ? field.group : field.group.title;
        if (!groups.has(title)) groups.set(title, []);
        const input = card.querySelector(`.plugin-field[data-key="${field.key}"]`);
        const present = field.secret ? input?.dataset.configured === "true"
            : String(settings?.[field.key] ?? "").trim().length > 0;
        groups.get(title).push(present);
    }
    for (const group of card.querySelectorAll(".plugin-group")) {
        const values = groups.get(group.dataset.group) || [];
        const count = values.filter(Boolean).length;
        const label = group.querySelector(".plugin-group-status");
        if (label) label.textContent = t(count === 0 ? "plugin.group_empty"
            : count === values.length ? "plugin.group_ready" : "plugin.group_partial");
    }
}

export function bindCredentialFields(card, plugin, schema, enqueue, flash) {
    const refreshGroups = () => refreshCredentialGroups(card, schema, plugin.settings);
    for (const field of schema) {
        const input = card.querySelector(`.plugin-field[data-key="${field.key}"]`);
        if (!input || field.type !== "string") continue;
        if (field.secret) {
            let hintRevision = 0;
            const refreshHint = async () => {
                try {
                    const token = ++hintRevision;
                    const hint = await invoke("get_plugin_secret_hint", {pluginId: plugin.id, fieldKey: field.key});
                    if (!input.isConnected || token !== hintRevision) return;
                    input.placeholder = hint ? `${hint} — ${t("plugin.secret_keep_hint")}` : t("plugin.secret_placeholder");
                    input.dataset.configured = hint ? "true" : "false";
                    refreshGroups();
                } catch (error) { console.error("get_plugin_secret_hint failed:", error); }
            };
            input._refreshHint = refreshHint;
            void refreshHint();
            const button = card.querySelector(`.plugin-secret-clear[data-key="${field.key}"]`);
            button?.addEventListener("click", async () => {
                button.disabled = true;
                const draft = input.value;
                await enqueue(async () => {
                    await invoke("clear_plugin_secret", {pluginId: plugin.id, fieldKey: field.key,
                        expectedRevision: plugin.revision ?? 0});
                    plugin.revision = (plugin.revision ?? 0) + 1;
                    plugin.settings = {...plugin.settings, [field.key]: ""};
                    if (input.value === draft) input.value = "";
                    input._refreshSpaceHint?.();
                    await refreshHint();
                    flash(card, t("plugin.secret_cleared"), false, 1500);
                });
                button.disabled = false;
            });
        }
        if (field.no_space) {
            const box = card.querySelector(`.plugin-field-warn[data-warn-key="${field.key}"]`);
            const refresh = () => {
                const dirty = /\s/.test(input.value ?? "");
                box?.classList.toggle("hidden", !dirty);
                const label = box?.querySelector(".plugin-field-warn-text");
                if (label) label.textContent = dirty ? t("plugin.field_space_hint") : "";
            };
            input._refreshSpaceHint = refresh;
            input.addEventListener("input", refresh);
            box?.querySelector(".plugin-field-warn-fix")?.addEventListener("click", () => {
                input.value = (input.value ?? "").replace(/\s+/g, "");
                input.focus({preventScroll: true});
                input.setSelectionRange?.(input.value.length, input.value.length);
                input.scrollLeft = input.scrollWidth;
                refresh();
                input.dispatchEvent(new Event("change", {bubbles: true}));
            });
            refresh();
        }
    }
    refreshGroups();
}
