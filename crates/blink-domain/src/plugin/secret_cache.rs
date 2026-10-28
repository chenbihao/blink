//! 仅宿主执行路径使用的插件凭据缓存；不进入配置快照、serde 或设置页明文。
use std::collections::HashMap;
use std::sync::Mutex;

use blink_infra::platform::secret::{SecretString, format_hint};
use serde_json::Value;

use super::secret_update::SecretStore;

struct CachedSecrets {
    revision: u64,
    generation: u64,
    values: Vec<(String, Option<SecretString>)>,
}

#[derive(Default)]
pub(super) struct PluginSecretCache(Mutex<HashMap<String, CachedSecrets>>);

impl PluginSecretCache {
    pub fn invalidate(&self, id: &str) {
        self.0.lock().unwrap().remove(id);
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    /// 调用者持有配置提交锁；同 revision/generation 只读一次 CM，包括“未配置”。
    pub fn restore(
        &self,
        id: &str,
        revision: u64,
        generation: u64,
        keys: &[String],
        settings: &mut Value,
        store: &dyn SecretStore,
    ) {
        let Some(obj) = settings.as_object_mut() else { return };
        let mut cache = self.0.lock().unwrap();
        let matches = cache.get(id).is_some_and(|entry|
            entry.revision == revision && entry.generation == generation
                && entry.values.iter().map(|(key, _)| key).eq(keys.iter()));
        if !matches {
            cache.remove(id);
            let mut values = Vec::with_capacity(keys.len());
            let mut complete = true;
            for key in keys {
                match store.read(id, key) {
                    Ok(value) => values.push((key.clone(), value)),
                    Err(_) => {
                        complete = false;
                        tracing::warn!(plugin = id, field = key, "插件凭据读取失败，下次调用重试");
                    }
                }
            }
            if !complete {
                // 保持已有的部分成功行为，但不把临时读取失败永久当成“未配置”。
                for (key, value) in &values {
                    if let Some(value) = value {
                        obj.insert(key.clone(), Value::String(value.expose().to_owned()));
                    }
                }
                return;
            }
            cache.insert(id.to_owned(), CachedSecrets {revision, generation, values});
            tracing::debug!(plugin = id, revision, generation, "插件凭据内存快照已加载");
        }
        for (key, value) in &cache[id].values {
            if let Some(value) = value {
                obj.insert(key.clone(), Value::String(value.expose().to_owned()));
            }
        }
    }

    /// 双层 Option：外层 None = 冷缓存；内层 None = 已确认未配置。只返回掩码。
    pub fn hint(&self, id: &str, revision: u64, generation: u64, key: &str) -> Option<Option<String>> {
        let cache = self.0.lock().unwrap();
        let entry = cache.get(id)?;
        if entry.revision != revision || entry.generation != generation { return None; }
        entry.values.iter().find(|(field, _)| field == key)
            .map(|(_, secret)| secret.as_ref().map(|secret| format_hint(secret.expose())))
    }
}
