/** 准备态只借用透明桌面输入；串行开关使迟到返回只释放原租约。 */
export function createPreparationLease(invoke) {
    let tail = Promise.resolve(), current = null;
    return {
        open() {
            if (current) return current;
            current = tail.catch(() => {}).then(() => invoke('gif_overlay_prepare', {active: true}));
            tail = current;
            return current;
        },
        ready() { return current || Promise.resolve(null); },
        close() {
            const original = current;
            current = null;
            if (!original) return tail;
            tail = tail.catch(() => {}).then(async () => {
                const id = await original.catch(() => null);
                if (id != null) await invoke('gif_overlay_prepare', {active: false, preparationId: id});
            });
            return tail;
        },
    };
}
