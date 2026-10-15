import {invoke} from "../../shared/tauri.js";
import {iconHTML} from "../../shared/icon.js";
import {t} from "../../i18n/index.js";

const SEARCH_DEBOUNCE_MS = 350;
const SEARCH_LIMIT = 10;
const NO_DRAFT = Symbol("no weather location draft");
export const WEATHER_EDITOR_CLOSED = "weather-editor-closed";

function escapeHtml(value) {
    return String(value ?? "").replace(/[&<>"']/g, (char) => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
    })[char]);
}

function formatMessage(key, values = {}) {
    let message = t(key);
    for (const [name, value] of Object.entries(values)) {
        message = message.split(`{${name}}`).join(String(value));
    }
    return message;
}

function getSettings(plugin) {
    return plugin?.settings && typeof plugin.settings === "object" ? plugin.settings : {};
}

/** 只把已明确暂存的地点写入配置；null 表示明确清除。
 *  天气来源（default_provider）由通用设置控件管理，此处不写死。 */
export function buildWeatherSettingsPatch(location, hasDraft = true) {
    if (!hasDraft) return {};
    return {
        settings_version: 1,
        default_location: location,
        default_city: "",
    };
}

/** 异步搜索结果只有在编辑器会话仍有效时才能回写。 */
export function isWeatherSearchCurrent({
    disposed,
    editorOpen,
    enabled,
    revision,
    currentRevision,
    rootConnected,
}) {
    return !disposed && editorOpen && enabled && revision === currentRevision && rootConnected !== false;
}

function formatCoordinate(value) {
    const number = Number(value);
    if (!Number.isFinite(number)) return "—";
    const fixed = number.toFixed(5);
    if (Number(fixed) === 0) return "0";
    return fixed.replace(/\.?0+$/, "");
}

function locationKindLabel(kind) {
    const keys = {
        city_center: "weather.kind_city",
        district_center: "weather.kind_district",
        county_center: "weather.kind_county",
        manual: "weather.kind_manual",
    };
    return keys[kind] ? t(keys[kind]) : String(kind || t("weather.kind_unknown"));
}

function locationSourceLabel(source) {
    if (source === "open_meteo") return t("weather.source_open_meteo");
    if (source === "qweather") return t("weather.source_qweather");
    if (source === "user") return t("weather.source_user");
    return String(source || t("weather.source_unknown"));
}

function renderLocationDetails(location, {candidate = false} = {}) {
    if (!location || typeof location !== "object") return "";

    const name = String(location.name || location.display_name || t("weather.location_unknown"));
    const displayName = String(location.display_name || name);
    const areas = Array.isArray(location.administrative_areas)
        ? location.administrative_areas.filter((area) => typeof area === "string" && area.trim())
        : [];
    const administrativeArea = areas.length
        ? areas.join(" · ")
        : String(location.administrative_area || "");
    const country = String(location.country || "");
    const countryCode = String(location.country_code || "");
    const countryText = [country, countryCode].filter(Boolean).join(" · ") || t("weather.country_unknown");
    const areaText = administrativeArea || t("weather.admin_unknown");
    const sourceText = locationSourceLabel(location.source);
    const kindText = locationKindLabel(location.kind);
    const coordinates = `${formatCoordinate(location.latitude)}, ${formatCoordinate(location.longitude)}`;

    return `<div class="weather-location-details${candidate ? " is-candidate" : ""}">
      <div class="weather-location-name"><strong>${escapeHtml(name)}</strong><span>${escapeHtml(displayName)}</span></div>
      <div class="weather-location-meta">
        <span>${escapeHtml(t("weather.detail_admin"))}：${escapeHtml(areaText)}</span>
        <span>${escapeHtml(t("weather.detail_country"))}：${escapeHtml(countryText)}</span>
        <span>${escapeHtml(t("weather.detail_kind"))}：${escapeHtml(kindText)}</span>
        <span>${escapeHtml(t("weather.detail_coordinates"))}：WGS84 ${escapeHtml(coordinates)}</span>
        <span>${escapeHtml(t("weather.detail_source"))}：${escapeHtml(sourceText)}</span>
      </div>
    </div>`;
}

function renderSavedLocation(settings) {
    const location = settings.default_location;
    if (location && typeof location === "object") {
        return renderLocationDetails(location);
    }

    const legacyCity = typeof settings.default_city === "string" ? settings.default_city.trim() : "";
    if (legacyCity) {
        return `<div class="weather-legacy-location">
          <strong>${escapeHtml(formatMessage("weather.legacy_city", {city: legacyCity}))}</strong>
          <p>${escapeHtml(t("weather.legacy_hint"))}</p>
        </div>`;
    }
    return `<p class="weather-location-empty">${escapeHtml(t("weather.location_none"))}</p>`;
}

/** 返回天气卡片中默认地点的摘要与内嵌编辑面板。 */
export function renderWeatherSettings(plugin) {
    const settings = getSettings(plugin);
    const enabled = plugin?.enabled !== false;
    const searchDisabled = enabled ? "" : " disabled";

    return `<section class="weather-settings" aria-label="${escapeHtml(t("weather.location_title"))}">
      <div class="weather-location-summary">
        <div class="weather-location-heading">${iconHTML("pin")}<strong>${escapeHtml(t("weather.location_title"))}</strong></div>
        <div class="weather-location-current" data-weather-current>${renderSavedLocation(settings)}</div>
        <button type="button" class="weather-edit-toggle" aria-expanded="false">
          ${iconHTML("pin")}<span>${escapeHtml(t("weather.edit_location"))}</span>
        </button>
      </div>

      <div class="weather-location-editor hidden" data-weather-editor aria-hidden="true">
        <div class="weather-editor-heading">
          <h4>${escapeHtml(t("weather.editor_title"))}</h4>
          <p>${escapeHtml(t("weather.editor_help"))}</p>
        </div>

        <div class="weather-search-section">
          <div class="weather-search-fields">
            <label class="weather-field weather-query-field">
              <span>${escapeHtml(t("weather.search_label"))}</span>
              <input class="weather-query" type="search" autocomplete="off" placeholder="${escapeHtml(t("weather.search_placeholder"))}"${searchDisabled} />
            </label>
            <label class="weather-field">
              <span>${escapeHtml(t("weather.country_code_label"))}</span>
              <input class="weather-country-code" type="text" maxlength="2" autocomplete="off" placeholder="${escapeHtml(t("weather.country_code_placeholder"))}"${searchDisabled} />
            </label>
            <label class="weather-field">
              <span>${escapeHtml(t("weather.admin_label"))}</span>
              <input class="weather-admin" type="text" autocomplete="off" placeholder="${escapeHtml(t("weather.admin_placeholder"))}"${searchDisabled} />
            </label>
          </div>
          <label class="weather-field weather-resolver-field">
            <span>${escapeHtml(t("weather.resolver_label"))}</span>
            <select class="weather-resolver"${searchDisabled}>
              <option value="auto">${escapeHtml(t("weather.resolver_auto"))}</option>
              <option value="open_meteo">${escapeHtml(t("weather.source_open_meteo"))}</option>
              <option value="qweather">${escapeHtml(t("weather.source_qweather"))}</option>
            </select>
          </label>
          <p class="weather-search-status" data-weather-search-status aria-live="polite">${escapeHtml(enabled ? t("weather.search_idle") : t("weather.search_disabled"))}</p>
          <div class="weather-candidates" data-weather-candidates role="listbox" aria-label="${escapeHtml(t("weather.candidates_label"))}"></div>
        </div>

        <details class="weather-manual-section">
          <summary>${iconHTML("globe")}<span>${escapeHtml(t("weather.manual_title"))}</span></summary>
          <p>${escapeHtml(t("weather.manual_help"))}</p>
          <div class="weather-manual-fields">
            <label class="weather-field">
              <span>${escapeHtml(t("weather.latitude_label"))}</span>
              <input class="weather-latitude" type="number" inputmode="decimal" step="any" min="-90" max="90" placeholder="${escapeHtml(t("weather.latitude_placeholder"))}" />
            </label>
            <label class="weather-field">
              <span>${escapeHtml(t("weather.longitude_label"))}</span>
              <input class="weather-longitude" type="number" inputmode="decimal" step="any" min="-180" max="180" placeholder="${escapeHtml(t("weather.longitude_placeholder"))}" />
            </label>
            <label class="weather-field weather-name-field">
              <span>${escapeHtml(t("weather.manual_name_label"))}</span>
              <input class="weather-manual-name" type="text" autocomplete="off" />
            </label>
          </div>
          <div class="weather-manual-actions">
            <span class="weather-manual-status" data-weather-manual-status aria-live="polite"></span>
            <button type="button" class="weather-manual-use">${iconHTML("check")}<span>${escapeHtml(t("weather.manual_use"))}</span></button>
          </div>
        </details>

        <div class="weather-editor-actions">
          <button type="button" class="weather-clear-location">${iconHTML("trash-2")}<span>${escapeHtml(t("weather.clear_location"))}</span></button>
          <span class="weather-action-spacer"></span>
          <button type="button" class="weather-cancel">${escapeHtml(t("weather.cancel"))}</button>
          <button type="button" class="weather-save">${iconHTML("save")}<span>${escapeHtml(t("weather.save"))}</span></button>
        </div>
      </div>
    </section>`;
}

function normalizeCandidate(candidate) {
    if (!candidate || typeof candidate !== "object") return null;
    const latitude = candidate.latitude;
    const longitude = candidate.longitude;
    const name = typeof candidate.name === "string" ? candidate.name.trim() : "";
    const displayName = typeof candidate.display_name === "string" ? candidate.display_name.trim() : "";
    const allowedKind = candidate.kind === "city_center"
        || candidate.kind === "district_center"
        || candidate.kind === "county_center";
    const source = candidate.source === "open_meteo" || candidate.source === "qweather"
        ? candidate.source
        : null;
    if (!name || !displayName || typeof latitude !== "number" || typeof longitude !== "number"
        || !Number.isFinite(latitude) || !Number.isFinite(longitude)
        || latitude < -90 || latitude > 90 || longitude < -180 || longitude > 180
        || candidate.coordinate_system !== "wgs84" || !source
        || !allowedKind || typeof candidate.resolved_at !== "string" || !candidate.resolved_at) {
        return null;
    }

    return {
        name,
        display_name: displayName,
        latitude,
        longitude,
        coordinate_system: "wgs84",
        country: typeof candidate.country === "string" ? candidate.country : "",
        country_code: typeof candidate.country_code === "string" ? candidate.country_code : null,
        administrative_area: typeof candidate.administrative_area === "string" ? candidate.administrative_area : "",
        administrative_areas: Array.isArray(candidate.administrative_areas)
            ? candidate.administrative_areas.filter((area) => typeof area === "string")
            : [],
        timezone: typeof candidate.timezone === "string" ? candidate.timezone : null,
        kind: candidate.kind,
        source,
        source_id: typeof candidate.source_id === "string" ? candidate.source_id : null,
        resolved_at: candidate.resolved_at,
    };
}

function createManualLocation(latitude, longitude, name) {
    const cleanName = name.trim();
    const coordinateName = `${formatCoordinate(latitude)}, ${formatCoordinate(longitude)}`;
    const displayName = cleanName || coordinateName;
    return {
        name: displayName,
        display_name: displayName,
        latitude,
        longitude,
        coordinate_system: "wgs84",
        country: "",
        country_code: null,
        administrative_area: "",
        administrative_areas: [],
        timezone: null,
        kind: "manual",
        source: "user",
        source_id: null,
        resolved_at: new Date().toISOString(),
    };
}

/** 挂载搜索、草稿、手工坐标和键盘交互；持久化仍由宿主保存整张插件卡片。 */
export function mountWeatherSettings(card, plugin) {
    const root = card?.querySelector?.(".weather-settings");
    if (!root) {
        return {collect: () => ({}), dispose() {}, setEnabled() {}};
    }

    const settings = getSettings(plugin);
    const editor = root.querySelector("[data-weather-editor]");
    const currentView = root.querySelector("[data-weather-current]");
    const candidatesView = root.querySelector("[data-weather-candidates]");
    const searchStatus = root.querySelector("[data-weather-search-status]");
    const manualStatus = root.querySelector("[data-weather-manual-status]");
    const queryInput = root.querySelector(".weather-query");
    const countryInput = root.querySelector(".weather-country-code");
    const adminInput = root.querySelector(".weather-admin");
    const resolverSelect = root.querySelector(".weather-resolver");
    const latitudeInput = root.querySelector(".weather-latitude");
    const longitudeInput = root.querySelector(".weather-longitude");
    const manualNameInput = root.querySelector(".weather-manual-name");

    let enabled = plugin?.enabled !== false;
    let composing = false;
    let disposed = false;
    let editorOpen = false;
    let draftLocation = NO_DRAFT;
    let searchRevision = 0;
    let searchTimer = null;
    let candidates = [];
    let activeCandidateIndex = -1;

    function setSearchStatus(key, values = {}, state = "") {
        if (!searchStatus) return;
        searchStatus.textContent = formatMessage(key, values);
        searchStatus.classList.toggle("is-error", state === "error");
        searchStatus.classList.toggle("is-loading", state === "loading");
    }

    function setManualStatus(key = "", state = "") {
        if (!manualStatus) return;
        manualStatus.textContent = key ? t(key) : "";
        manualStatus.classList.toggle("is-error", state === "error");
        manualStatus.classList.toggle("is-success", state === "success");
    }

    function invalidateSearch({clearCandidates = true} = {}) {
        searchRevision += 1;
        if (searchTimer !== null) {
            clearTimeout(searchTimer);
            searchTimer = null;
        }
        if (clearCandidates) {
            candidates = [];
            activeCandidateIndex = -1;
            renderCandidates();
        }
        return searchRevision;
    }

    function isCurrentSearch(revision) {
        return isWeatherSearchCurrent({
            disposed,
            editorOpen,
            enabled,
            revision,
            currentRevision: searchRevision,
            rootConnected: root.isConnected,
        });
    }

    function renderCandidates() {
        if (!candidatesView) return;
        candidatesView.innerHTML = candidates.map((candidate, index) => `
          <button type="button" class="weather-candidate${index === activeCandidateIndex ? " is-active" : ""}"
            role="option" aria-selected="${index === activeCandidateIndex ? "true" : "false"}"
            data-weather-candidate-index="${index}">
            ${renderLocationDetails(candidate, {candidate: true})}
            <span class="weather-candidate-action">${iconHTML("check")}<span>${escapeHtml(t("weather.choose_candidate"))}</span></span>
          </button>`).join("");
        candidatesView.classList.toggle("has-candidates", candidates.length > 0);
    }

    function refreshCurrentView() {
        if (!currentView) return;
        if (draftLocation === NO_DRAFT) {
            currentView.innerHTML = renderSavedLocation(settings);
            return;
        }
        if (draftLocation === null) {
            currentView.innerHTML = `<p class="weather-location-pending">${escapeHtml(t("weather.clear_pending"))}</p>`;
            return;
        }
        currentView.innerHTML = `${renderLocationDetails(draftLocation)}<p class="weather-location-pending">${escapeHtml(t("weather.draft_pending"))}</p>`;
    }

    function setEditorOpen(open) {
        editorOpen = open;
        editor?.classList.toggle("hidden", !open);
        editor?.setAttribute("aria-hidden", open ? "false" : "true");
        const toggle = root.querySelector(".weather-edit-toggle");
        toggle?.setAttribute("aria-expanded", open ? "true" : "false");
    }

    function clearManualInputs() {
        if (latitudeInput) latitudeInput.value = "";
        if (longitudeInput) longitudeInput.value = "";
        if (manualNameInput) manualNameInput.value = "";
        setManualStatus();
        root.querySelector(".weather-manual-section")?.removeAttribute("open");
    }

    function closeEditor() {
        const wasOpen = editorOpen;
        invalidateSearch();
        composing = false;
        const activeElement = root.ownerDocument?.activeElement;
        if (activeElement && editor?.contains(activeElement)) activeElement.blur?.();
        setEditorOpen(false);
        draftLocation = NO_DRAFT;
        refreshCurrentView();
        if (queryInput) queryInput.value = "";
        if (countryInput) countryInput.value = "";
        if (adminInput) adminInput.value = "";
        if (resolverSelect) resolverSelect.value = "auto";
        clearManualInputs();
        setSearchStatus(enabled ? "weather.search_idle" : "weather.search_disabled");
        const CustomEventType = root.ownerDocument?.defaultView?.CustomEvent ?? globalThis.CustomEvent;
        if (wasOpen && CustomEventType) {
            root.dispatchEvent(new CustomEventType(WEATHER_EDITOR_CLOSED, {bubbles: true}));
        }
    }

    function openEditor() {
        setEditorOpen(true);
        setSearchStatus(enabled ? "weather.search_idle" : "weather.search_disabled");
        queryInput?.focus({preventScroll: true});
    }

    async function runSearch(revision) {
        if (!isCurrentSearch(revision)) return;
        const query = queryInput?.value.trim() || "";
        if (!query) {
            setSearchStatus("weather.search_idle");
            return;
        }

        const countryCode = countryInput?.value.trim().toUpperCase() || "";
        const admin = adminInput?.value.trim() || "";
        const resolver = resolverSelect?.value || "auto";
        try {
            const result = await invoke("search_weather_locations", {
                query,
                countryCode: countryCode || null,
                admin: admin || null,
                limit: SEARCH_LIMIT,
                resolver,
            });
            if (!isCurrentSearch(revision)) return;
            // 后端返回实际使用的解析来源；auto 会落到其中之一。
            if (result?.resolver !== "open_meteo" && result?.resolver !== "qweather") {
                setSearchStatus("weather.search_unsupported", {}, "error");
                return;
            }
            const incoming = Array.isArray(result.candidates) ? result.candidates : [];
            const normalized = incoming.map(normalizeCandidate);
            if (normalized.some((candidate) => !candidate)) {
                setSearchStatus("weather.search_invalid", {}, "error");
                return;
            }
            candidates = normalized;
            activeCandidateIndex = candidates.length ? 0 : -1;
            renderCandidates();
            if (candidates.length) {
                setSearchStatus("weather.search_count", {count: Number(result.count ?? candidates.length)});
            } else {
                setSearchStatus("weather.search_empty");
            }
        } catch (error) {
            if (!isCurrentSearch(revision)) return;
            setSearchStatus("weather.search_error", {message: error?.message || String(error)}, "error");
        }
    }

    function scheduleSearch() {
        if (composing || disposed || !editorOpen) return;
        const revision = invalidateSearch();
        if (!enabled) {
            setSearchStatus("weather.search_disabled");
            return;
        }
        if (!queryInput?.value.trim()) {
            setSearchStatus("weather.search_idle");
            return;
        }
        setSearchStatus("weather.searching", {}, "loading");
        searchTimer = setTimeout(() => {
            searchTimer = null;
            runSearch(revision);
        }, SEARCH_DEBOUNCE_MS);
    }

    function chooseCandidate(index) {
        const location = candidates[index];
        if (!location || disposed) return;
        invalidateSearch();
        draftLocation = location;
        refreshCurrentView();
        setSearchStatus("weather.draft_selected");
    }

    function useManualLocation() {
        const latitudeText = latitudeInput?.value.trim() || "";
        const longitudeText = longitudeInput?.value.trim() || "";
        const latitude = Number(latitudeText);
        const longitude = Number(longitudeText);
        if (!latitudeText || !longitudeText || !Number.isFinite(latitude) || !Number.isFinite(longitude)
            || latitude < -90 || latitude > 90 || longitude < -180 || longitude > 180) {
            setManualStatus("weather.manual_error", "error");
            return;
        }
        invalidateSearch();
        draftLocation = createManualLocation(latitude, longitude, manualNameInput?.value || "");
        refreshCurrentView();
        setManualStatus("weather.manual_selected", "success");
    }

    function markClearLocation() {
        invalidateSearch();
        draftLocation = null;
        refreshCurrentView();
        setSearchStatus("weather.clear_pending");
    }

    function handleClick(event) {
        const target = event.target.closest("button, summary");
        if (!target || !root.contains(target)) return;
        if (target.matches(".weather-edit-toggle")) {
            if (editorOpen) closeEditor();
            else openEditor();
            return;
        }
        if (target.matches(".weather-cancel")) {
            closeEditor();
            return;
        }
        if (target.matches(".weather-clear-location")) {
            markClearLocation();
            return;
        }
        if (target.matches(".weather-manual-use")) {
            useManualLocation();
            return;
        }
        if (target.matches(".weather-candidate")) {
            chooseCandidate(Number(target.dataset.weatherCandidateIndex));
        }
    }

    function handleInput(event) {
        if (event.target.matches(".weather-query, .weather-country-code, .weather-admin")) {
            scheduleSearch();
            return;
        }
        if (event.target.matches(".weather-latitude, .weather-longitude, .weather-manual-name")) {
            setManualStatus();
        }
    }

    /** 切换解析来源立即使旧候选与在途请求失效，不回流旧来源的结果。 */
    function handleResolverChange() {
        invalidateSearch();
        if (!editorOpen) return;
        if (!queryInput?.value.trim()) {
            setSearchStatus(enabled ? "weather.search_idle" : "weather.search_disabled");
            return;
        }
        scheduleSearch();
    }

    function handleCompositionStart(event) {
        if (!event.target.matches(".weather-query, .weather-country-code, .weather-admin")) return;
        composing = true;
        invalidateSearch();
        setSearchStatus(enabled ? "weather.search_idle" : "weather.search_disabled");
    }

    function handleCompositionEnd(event) {
        if (!event.target.matches(".weather-query, .weather-country-code, .weather-admin")) return;
        composing = false;
        scheduleSearch();
    }

    function handleKeydown(event) {
        if (event.isComposing || composing || event.keyCode === 229) return;
        if (event.key === "Escape" && editorOpen && editor?.contains(event.target)) {
            event.preventDefault();
            if (candidates.length) {
                invalidateSearch();
                queryInput?.focus({preventScroll: true});
                setSearchStatus("weather.candidates_closed");
            } else {
                closeEditor();
            }
            return;
        }
        if (!event.target.matches(".weather-query, .weather-country-code, .weather-admin")) return;
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
            if (!candidates.length) return;
            event.preventDefault();
            const delta = event.key === "ArrowDown" ? 1 : -1;
            activeCandidateIndex = (activeCandidateIndex + delta + candidates.length) % candidates.length;
            renderCandidates();
            return;
        }
        if (event.key === "Enter") {
            if (!candidates.length) return;
            event.preventDefault();
            chooseCandidate(activeCandidateIndex >= 0 ? activeCandidateIndex : 0);
            return;
        }
    }

    root.addEventListener("click", handleClick);
    root.addEventListener("input", handleInput);
    root.addEventListener("change", (event) => {
        if (event.target.matches(".weather-resolver")) handleResolverChange();
    });
    root.addEventListener("compositionstart", handleCompositionStart);
    root.addEventListener("compositionend", handleCompositionEnd);
    root.addEventListener("keydown", handleKeydown);

    function collect() {
        if (disposed || draftLocation === NO_DRAFT) return {};
        return buildWeatherSettingsPatch(draftLocation);
    }

    function setEnabled(value) {
        enabled = value === true;
        composing = false;
        const searchFields = [queryInput, countryInput, adminInput];
        searchFields.forEach((field) => {
            if (field) field.disabled = !enabled;
        });
        if (resolverSelect) resolverSelect.disabled = !enabled;
        invalidateSearch();
        if (editorOpen) setSearchStatus(enabled ? "weather.search_idle" : "weather.search_disabled");
    }

    function dispose() {
        if (disposed) return;
        disposed = true;
        invalidateSearch();
        root.removeEventListener("click", handleClick);
        root.removeEventListener("input", handleInput);
        root.removeEventListener("compositionstart", handleCompositionStart);
        root.removeEventListener("compositionend", handleCompositionEnd);
        root.removeEventListener("keydown", handleKeydown);
    }

    renderCandidates();
    return {collect, dispose, setEnabled};
}
