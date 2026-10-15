//! Transaction-like updates for a plugin's Credential Manager secrets.
//!
//! The caller owns serialization and persistence CAS. This helper snapshots all
//! current values before writing any update, then restores touched fields if a
//! write fails or the returned guard is dropped without being committed.

use blink_infra::platform::secret::SecretString;

const READ_FAILED: &str = "secret_read_failed";
const WRITE_FAILED: &str = "secret_write_failed";
const RESTORE_FAILED: &str = "secret_restore_failed";

pub(super) trait SecretStore: Send + Sync {
    fn read(&self, plugin_id: &str, key: &str) -> Result<Option<SecretString>, String>;
    fn write(&self, plugin_id: &str, key: &str, value: Option<&SecretString>)
    -> Result<(), String>;
}

/// Adapter for the production Credential Manager backed secret store.
pub(super) struct PlatformSecretStore;

impl SecretStore for PlatformSecretStore {
    fn read(&self, plugin_id: &str, key: &str) -> Result<Option<SecretString>, String> {
        #[cfg(target_os = "windows")]
        {
            use blink_infra::platform::secret::{SecretError, SecretRef, load_secret};

            match load_secret(&SecretRef::plugin(plugin_id, key)) {
                Ok(secret) => Ok(Some(secret)),
                Err(SecretError::NotFound(_)) => Ok(None),
                Err(error) => Err(format!("{READ_FAILED}: {error}")),
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = (plugin_id, key);
            Err("platform_secret_store_unavailable".to_string())
        }
    }

    fn write(
        &self,
        plugin_id: &str,
        key: &str,
        value: Option<&SecretString>,
    ) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        {
            use blink_infra::platform::secret::{
                SecretError, SecretRef, delete_secret, save_secret,
            };

            let reference = SecretRef::plugin(plugin_id, key);
            match value {
                Some(secret) => save_secret(&reference, secret)
                    .map_err(|error| format!("{WRITE_FAILED}: {error}")),
                None => match delete_secret(&reference) {
                    Ok(()) | Err(SecretError::NotFound(_)) => Ok(()),
                    Err(error) => Err(format!("{WRITE_FAILED}: {error}")),
                },
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            let _ = (plugin_id, key, value);
            Err("platform_secret_store_unavailable".to_string())
        }
    }
}

/// Rollback guard for a set of plugin secret updates.
///
/// Deliberately has no `Debug` implementation so callers cannot accidentally
/// include its saved values in logs.
pub(super) struct SecretUpdate<'a> {
    store: &'a dyn SecretStore,
    plugin_id: String,
    old_values: Vec<(String, Option<SecretString>)>,
    finished: bool,
}

impl<'a> SecretUpdate<'a> {
    /// Keep the applied values and discard the saved rollback snapshot.
    pub fn commit(mut self) {
        self.finished = true;
    }

    /// Restore every saved value and report a failed restoration to the caller.
    pub fn rollback(mut self) -> Result<(), String> {
        self.finished = true;
        restore_values(self.store, &self.plugin_id, &self.old_values)
            .map_err(|()| RESTORE_FAILED.to_string())
    }
}

/// Apply updates after snapshotting every old value.
pub(super) fn apply<'a>(
    store: &'a dyn SecretStore,
    plugin_id: &str,
    updates: Vec<(String, Option<SecretString>)>,
) -> Result<SecretUpdate<'a>, String> {
    let mut old_values = Vec::with_capacity(updates.len());
    for (key, _) in &updates {
        let old = store
            .read(plugin_id, key)
            .map_err(|_| READ_FAILED.to_string())?;
        old_values.push((key.clone(), old));
    }

    for (index, (key, value)) in updates.iter().enumerate() {
        if store.write(plugin_id, key, value.as_ref()).is_err() {
            // Count the failed write too: a store may have partially applied it.
            let touched = &old_values[..=index];
            return match restore_values(store, plugin_id, touched) {
                Ok(()) => Err(WRITE_FAILED.to_string()),
                Err(()) => Err(format!("{RESTORE_FAILED}: {WRITE_FAILED}")),
            };
        }
    }

    Ok(SecretUpdate {
        store,
        plugin_id: plugin_id.to_string(),
        old_values,
        finished: false,
    })
}

impl Drop for SecretUpdate<'_> {
    fn drop(&mut self) {
        if !self.finished && restore_values(self.store, &self.plugin_id, &self.old_values).is_err()
        {
            // Never log values, keys, or backend-provided error strings here.
            tracing::warn!(error = RESTORE_FAILED, "插件密钥事务自动回滚失败");
        }
    }
}

fn restore_values(
    store: &dyn SecretStore,
    plugin_id: &str,
    values: &[(String, Option<SecretString>)],
) -> Result<(), ()> {
    let mut failed = false;
    for (key, value) in values.iter().rev() {
        if store.write(plugin_id, key, value.as_ref()).is_err() {
            // Keep restoring the other fields so one backend error does not
            // prevent best-effort recovery of the remaining snapshot.
            failed = true;
        }
    }
    if failed { Err(()) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::{SecretStore, apply};
    use blink_infra::platform::secret::SecretString;
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemoryState {
        values: HashMap<(String, String), SecretString>,
        read_calls: usize,
        write_calls: usize,
        fail_read_at: Option<usize>,
        fail_write_before: HashSet<usize>,
        fail_write_after: HashSet<usize>,
    }

    #[derive(Default)]
    struct MemorySecretStore(Mutex<MemoryState>);

    impl MemorySecretStore {
        fn seed(&self, plugin_id: &str, key: &str, value: &str) {
            self.0.lock().unwrap().values.insert(
                (plugin_id.to_string(), key.to_string()),
                SecretString::new(value.to_string()),
            );
        }

        fn value(&self, plugin_id: &str, key: &str) -> Option<String> {
            self.0
                .lock()
                .unwrap()
                .values
                .get(&(plugin_id.to_string(), key.to_string()))
                .map(|secret| secret.expose().to_string())
        }

        fn fail_read_at(&self, call: usize) {
            self.0.lock().unwrap().fail_read_at = Some(call);
        }

        fn fail_write_before(&self, call: usize) {
            self.0.lock().unwrap().fail_write_before.insert(call);
        }

        fn fail_write_after(&self, call: usize) {
            self.0.lock().unwrap().fail_write_after.insert(call);
        }

        fn write_calls(&self) -> usize {
            self.0.lock().unwrap().write_calls
        }
    }

    impl SecretStore for MemorySecretStore {
        fn read(&self, plugin_id: &str, key: &str) -> Result<Option<SecretString>, String> {
            let mut state = self.0.lock().unwrap();
            state.read_calls += 1;
            if state.fail_read_at == Some(state.read_calls) {
                return Err("injected read failure".to_string());
            }
            Ok(state
                .values
                .get(&(plugin_id.to_string(), key.to_string()))
                .map(|value| SecretString::new(value.expose().to_string())))
        }

        fn write(
            &self,
            plugin_id: &str,
            key: &str,
            value: Option<&SecretString>,
        ) -> Result<(), String> {
            let mut state = self.0.lock().unwrap();
            state.write_calls += 1;
            let call = state.write_calls;
            if state.fail_write_before.contains(&call) {
                return Err("injected write failure".to_string());
            }

            let map_key = (plugin_id.to_string(), key.to_string());
            match value {
                Some(secret) => {
                    state
                        .values
                        .insert(map_key, SecretString::new(secret.expose().to_string()));
                }
                None => {
                    state.values.remove(&map_key);
                }
            }

            if state.fail_write_after.contains(&call) {
                return Err("injected partial write failure".to_string());
            }
            Ok(())
        }
    }

    #[test]
    fn applies_multiple_updates_and_commit_keeps_them() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "first", "old-first");
        store.seed("plugin.test", "second", "old-second");

        let update = apply(
            &store,
            "plugin.test",
            vec![
                ("first".to_string(), Some(SecretString::new("new-first"))),
                ("second".to_string(), Some(SecretString::new("new-second"))),
            ],
        )
        .unwrap();

        assert_eq!(
            store.value("plugin.test", "first").as_deref(),
            Some("new-first")
        );
        assert_eq!(
            store.value("plugin.test", "second").as_deref(),
            Some("new-second")
        );
        update.commit();
        assert_eq!(
            store.value("plugin.test", "first").as_deref(),
            Some("new-first")
        );
        assert_eq!(
            store.value("plugin.test", "second").as_deref(),
            Some("new-second")
        );
    }

    #[test]
    fn failed_partially_applied_write_restores_touched_values() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "first", "old-first");
        store.seed("plugin.test", "second", "old-second");
        store.fail_write_after(2);

        let error = match apply(
            &store,
            "plugin.test",
            vec![
                ("first".to_string(), Some(SecretString::new("new-first"))),
                ("second".to_string(), Some(SecretString::new("new-second"))),
            ],
        ) {
            Err(error) => error,
            Ok(update) => {
                update.rollback().unwrap();
                panic!("预期密钥写入失败")
            }
        };

        assert_eq!(error, "secret_write_failed");
        assert_eq!(
            store.value("plugin.test", "first").as_deref(),
            Some("old-first")
        );
        assert_eq!(
            store.value("plugin.test", "second").as_deref(),
            Some("old-second")
        );
    }

    #[test]
    fn read_failure_happens_before_any_mutation() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "first", "old-first");
        store.seed("plugin.test", "second", "old-second");
        store.fail_read_at(2);

        let error = match apply(
            &store,
            "plugin.test",
            vec![
                ("first".to_string(), Some(SecretString::new("new-first"))),
                ("second".to_string(), Some(SecretString::new("new-second"))),
            ],
        ) {
            Err(error) => error,
            Ok(update) => {
                update.rollback().unwrap();
                panic!("预期密钥读取失败")
            }
        };

        assert_eq!(error, "secret_read_failed");
        assert_eq!(store.write_calls(), 0);
        assert_eq!(
            store.value("plugin.test", "first").as_deref(),
            Some("old-first")
        );
        assert_eq!(
            store.value("plugin.test", "second").as_deref(),
            Some("old-second")
        );
    }

    #[test]
    fn none_update_deletes_the_secret() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "token", "old-token");

        apply(&store, "plugin.test", vec![("token".to_string(), None)])
            .unwrap()
            .commit();

        assert_eq!(store.value("plugin.test", "token"), None);
    }

    #[test]
    fn dropping_uncommitted_update_restores_previous_value() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "token", "old-token");

        drop(
            apply(
                &store,
                "plugin.test",
                vec![("token".to_string(), Some(SecretString::new("new-token")))],
            )
            .unwrap(),
        );

        assert_eq!(
            store.value("plugin.test", "token").as_deref(),
            Some("old-token")
        );
    }

    #[test]
    fn explicit_rollback_restores_previous_value() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "token", "old-token");

        let update = apply(
            &store,
            "plugin.test",
            vec![("token".to_string(), Some(SecretString::new("new-token")))],
        )
        .unwrap();
        update.rollback().unwrap();

        assert_eq!(
            store.value("plugin.test", "token").as_deref(),
            Some("old-token")
        );
    }

    #[test]
    fn explicit_rollback_reports_restore_failure() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "token", "old-token");
        // The update is write #1; fail the explicit rollback write #2.
        store.fail_write_before(2);

        let update = apply(
            &store,
            "plugin.test",
            vec![("token".to_string(), Some(SecretString::new("new-token")))],
        )
        .unwrap();
        let error = update.rollback().unwrap_err();

        assert!(error.contains("secret_restore_failed"));
        assert_eq!(
            store.value("plugin.test", "token").as_deref(),
            Some("new-token")
        );
    }

    #[test]
    fn failed_restore_is_reported_explicitly() {
        let store = MemorySecretStore::default();
        store.seed("plugin.test", "first", "old-first");
        store.seed("plugin.test", "second", "old-second");
        // Update #2 partially applies and fails; restore #3 then fails before applying.
        store.fail_write_after(2);
        store.fail_write_before(3);

        let error = match apply(
            &store,
            "plugin.test",
            vec![
                ("first".to_string(), Some(SecretString::new("new-first"))),
                ("second".to_string(), Some(SecretString::new("new-second"))),
            ],
        ) {
            Err(error) => error,
            Ok(update) => {
                update.rollback().unwrap();
                panic!("预期密钥写入和恢复失败")
            }
        };

        assert!(error.contains("secret_restore_failed"));
    }
}
