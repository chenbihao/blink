import {t} from "../i18n/index.js";
import {formatBytes} from "../shared/download-progress.js";

/** Geometry uses CSS pixels; scale is relative to the image's natural dimensions. */
export function fitImage(width, height, viewportWidth, viewportHeight) {
    const scale = Math.min(1, viewportWidth / width, viewportHeight / height);
    return clampImage({width, height, viewportWidth, viewportHeight, scale, minScale: scale,
        x: 0, y: 0});
}

function clampImage(state) {
    const clamp = (value, image, viewport) => image <= viewport
        ? (viewport - image) / 2 : Math.max(viewport - image, Math.min(0, value));
    return {...state,
        x: clamp(state.x, state.width * state.scale, state.viewportWidth),
        y: clamp(state.y, state.height * state.scale, state.viewportHeight)};
}

export function zoomImage(state, factor, x, y) {
    const scale = Math.max(state.minScale, Math.min(4, state.scale * factor));
    const ratio = scale / state.scale;
    return clampImage({...state, scale, x: x - (x - state.x) * ratio, y: y - (y - state.y) * ratio});
}

export function panImage(state, dx, dy) {
    return clampImage({...state, x: state.x + dx, y: state.y + dy});
}

/** One loaded img is retained while zooming, including animated GIF/WebP. */
export function createClipboardImageViewer(image, {width, maxHeight, onDragStart, onDragEnd}) {
    const root = document.createElement("div");
    root.className = "clipboard-image-viewer";
    const viewport = document.createElement("div");
    viewport.className = "clipboard-image-viewport";
    const caption = document.createElement("div");
    caption.className = "clipboard-image-caption";
    const naturalWidth = image.naturalWidth, naturalHeight = image.naturalHeight;
    const height = Math.min(maxHeight, naturalHeight * Math.min(1, width / naturalWidth));
    let state = fitImage(naturalWidth, naturalHeight, width, height);
    let drag = null, sizeBytes = null;
    viewport.style.height = `${height}px`;
    image.style.width = `${naturalWidth}px`;
    image.style.height = `${naturalHeight}px`;
    image.draggable = false;
    viewport.appendChild(image);
    root.appendChild(viewport);
    root.appendChild(caption);
    const render = () => {
        image.style.transform = `translate(${state.x}px, ${state.y}px) scale(${state.scale})`;
        viewport.classList.toggle("is-zoomed", state.scale > state.minScale);
        caption.textContent = [Number.isFinite(sizeBytes) ? t("clipboard.preview_file_size", {size: formatBytes(sizeBytes)}) : "",
            `${Math.round(state.scale * 100)}%`, t("clipboard.preview_image_controls")].filter(Boolean).join(" · ");
        caption.title = caption.textContent;
    };
    const finishDrag = () => {
        if (!drag) return;
        const pointerId = drag.id;
        drag = null;
        viewport.classList.remove("is-dragging");
        if (viewport.hasPointerCapture?.(pointerId)) viewport.releasePointerCapture(pointerId);
        onDragEnd();
    };
    viewport.addEventListener("wheel", event => {
        event.preventDefault();
        event.stopPropagation();
        const rect = viewport.getBoundingClientRect();
        const delta = event.deltaY * (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? height : 1);
        state = zoomImage(state, Math.exp(-Math.max(-1000, Math.min(1000, delta)) * 0.002),
            event.clientX - rect.left, event.clientY - rect.top);
        render();
    }, {passive: false});
    viewport.addEventListener("pointerdown", event => {
        if (event.button !== 0 || drag) return;
        event.stopPropagation(); // Images must not enter the panel's text-selection gesture.
        event.preventDefault();
        if (state.scale <= state.minScale) return;
        drag = {id: event.pointerId, x: event.clientX, y: event.clientY};
        viewport.setPointerCapture(event.pointerId);
        viewport.classList.add("is-dragging");
        onDragStart();
    });
    viewport.addEventListener("pointermove", event => {
        if (!drag || event.pointerId !== drag.id) return;
        state = panImage(state, event.clientX - drag.x, event.clientY - drag.y);
        drag.x = event.clientX;
        drag.y = event.clientY;
        render();
    });
    for (const name of ["pointerup", "pointercancel", "lostpointercapture"]) {
        viewport.addEventListener(name, event => {if (drag?.id === event.pointerId) finishDrag();});
    }
    viewport.addEventListener("dblclick", event => {
        event.preventDefault();
        finishDrag();
        state = fitImage(naturalWidth, naturalHeight, width, height);
        render();
    });
    render();
    return {root, dispose: finishDrag, setFileSize(bytes) {sizeBytes = bytes; render();}};
}
