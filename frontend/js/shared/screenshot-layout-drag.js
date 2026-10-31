// Pointer-based sorting works inside WebView2 without native HTML drag/drop interception.
export function bindLayoutDrag({panel, lists, isSaving, onDrop}) {
    let gesture = null;
    const rows = group => [...group.children].filter(node => node.classList.contains('ss-layout-row'));
    function clearMarkers() {
        for (const group of lists.children) {
            group.classList.remove('is-drop-target');
            for (const row of rows(group)) row.classList.remove('ss-drop-before', 'ss-drop-after');
        }
    }
    function cancel() {
        if (!gesture) return false;
        const old = gesture;
        gesture = null;
        clearMarkers();
        old.row.classList.remove('dragging');
        old.preview?.remove();
        old.parent.insertBefore(old.row, old.nextSibling?.parentNode === old.parent ? old.nextSibling : null);
        panel.classList.remove('is-layout-dragging');
        if (lists.hasPointerCapture?.(old.pointerId)) lists.releasePointerCapture(old.pointerId);
        return true;
    }
    function targetAt(x, y) {
        const viewport = lists.getBoundingClientRect();
        if (x < viewport.left || x > viewport.right || y < viewport.top || y > viewport.bottom) return null;
        for (const group of lists.children) {
            const bounds = group.getBoundingClientRect();
            if (x < bounds.left || x > bounds.right || y < bounds.top || y > bounds.bottom) continue;
            const items = rows(group).filter(row => row !== gesture?.row);
            const next = items.find(row => {
                const rect = row.getBoundingClientRect();
                return y < rect.top + rect.height / 2;
            });
            return {group, row: next || items.at(-1), zone: group.dataset.zone, after: !next};
        }
        return null;
    }
    lists.addEventListener('pointerdown', event => {
        const row = event.target.closest?.('.ss-layout-row');
        if (!row || event.target.closest('button') || event.button !== 0 || event.isPrimary === false || isSaving()) return;
        cancel();
        event.preventDefault();
        const rect = row.getBoundingClientRect();
        gesture = {row, id: row.dataset.id, pointerId: event.pointerId, x: event.clientX, y: event.clientY,
            parent: row.parentNode, nextSibling: row.nextSibling,
            offsetX: event.clientX - rect.left, offsetY: event.clientY - rect.top, active: false, target: null};
        lists.setPointerCapture?.(event.pointerId);
    });
    lists.addEventListener('pointermove', event => {
        if (!gesture || gesture.pointerId !== event.pointerId) return;
        if (isSaving()) { cancel(); return; }
        if (!gesture.active && Math.hypot(event.clientX - gesture.x, event.clientY - gesture.y) < 5) return;
        event.preventDefault();
        if (!gesture.active) {
            const rect = gesture.row.getBoundingClientRect();
            const preview = gesture.row.cloneNode(true);
            preview.classList.add('sortable-drag-preview', 'ss-layout-drag-preview');
            preview.setAttribute('aria-hidden', 'true');
            const style = getComputedStyle(panel);
            for (const name of ['--ss-panel-control', '--ss-panel-text', '--ss-panel-border', '--ss-panel-hover']) {
                preview.style.setProperty(name, style.getPropertyValue(name));
            }
            // Keep the whole clone (icons, padding and controls included) at the source's visual scale.
            preview.style.width = `${gesture.row.offsetWidth}px`;
            preview.style.height = `${gesture.row.offsetHeight}px`;
            preview.style.font = getComputedStyle(gesture.row).font;
            preview.style.transformOrigin = 'top left';
            preview.style.transform = `scale(${rect.width / gesture.row.offsetWidth}, ${rect.height / gesture.row.offsetHeight})`;
            document.body.append(preview);
            gesture.preview = preview;
            gesture.active = true;
        }
        gesture.row.classList.add('dragging');
        gesture.preview.style.left = `${event.clientX - gesture.offsetX}px`;
        gesture.preview.style.top = `${event.clientY - gesture.offsetY}px`;
        panel.classList.add('is-layout-dragging');
        clearMarkers();
        gesture.target = targetAt(event.clientX, event.clientY);
        const target = gesture.target;
        if (target) {
            target.group.classList.add('is-drop-target');
            target.row?.classList.add(target.after ? 'ss-drop-after' : 'ss-drop-before');
            // Match plugin sorting: the dashed source card previews its final position live.
            target.group.insertBefore(gesture.row, target.after ? null : target.row || null);
        }
    });
    lists.addEventListener('pointerup', event => {
        if (!gesture || gesture.pointerId !== event.pointerId) return;
        const {active, id} = gesture;
        const target = active ? targetAt(event.clientX, event.clientY) : null;
        cancel();
        if (active && target && !isSaving()) {
            event.preventDefault();
            onDrop(id, target.zone, target.row?.dataset.id, target.after);
        }
    });
    for (const type of ['pointercancel', 'lostpointercapture']) lists.addEventListener(type, cancel);
    return {cancel};
}
