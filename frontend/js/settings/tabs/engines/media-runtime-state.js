/** 安装快照的代次/版本门禁；查询可以刷新同 revision 的磁盘状态。 */
export function mergeMediaRuntimeSnapshot(current, incoming) {
    if (!incoming?.epoch || !Number.isSafeInteger(incoming.revision) || !incoming.runtime) return current;
    if (current?.epoch === incoming.epoch && incoming.revision < current.revision) return current;
    return incoming;
}

export function mediaRuntimeAction(snapshot) {
    if (!snapshot || snapshot.operation) return null;
    const state = snapshot.runtime.state;
    if (state === 'missing') return 'install';
    if (state === 'ready') return 'diagnose';
    if (state === 'corrupt' || state === 'outdated') return 'repair';
    return null;
}
