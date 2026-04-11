use anyhow::{Context, Result};

pub trait SecretStore {
    fn save(&self, key: &str, value: &str) -> Result<()>;
    fn load(&self, key: &str) -> Result<Option<String>>;
    fn delete(&self, key: &str) -> Result<()>;
}

#[derive(Clone, Debug)]
pub struct KeyringSecretStore {
    service_name: &'static str,
}

impl Default for KeyringSecretStore {
    fn default() -> Self {
        Self {
            service_name: "codex-account-switcher",
        }
    }
}

impl SecretStore for KeyringSecretStore {
    fn save(&self, key: &str, value: &str) -> Result<()> {
        let entry = keyring::Entry::new(self.service_name, key)?;
        entry
            .set_password(value)
            .context("failed to write snapshot to keychain")
    }

    fn load(&self, key: &str) -> Result<Option<String>> {
        let entry = keyring::Entry::new(self.service_name, key)?;
        match entry.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(error).context("failed to load snapshot from keychain"),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        let entry = keyring::Entry::new(self.service_name, key)?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error).context("failed to delete snapshot from keychain"),
        }
    }
}

#[cfg(test)]
pub mod test_support {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use anyhow::Result;

    use super::SecretStore;

    #[derive(Clone, Default)]
    pub struct MemorySecretStore {
        inner: Arc<Mutex<HashMap<String, String>>>,
    }

    impl SecretStore for MemorySecretStore {
        fn save(&self, key: &str, value: &str) -> Result<()> {
            self.inner
                .lock()
                .expect("lock")
                .insert(key.to_owned(), value.to_owned());
            Ok(())
        }

        fn load(&self, key: &str) -> Result<Option<String>> {
            Ok(self.inner.lock().expect("lock").get(key).cloned())
        }

        fn delete(&self, key: &str) -> Result<()> {
            self.inner.lock().expect("lock").remove(key);
            Ok(())
        }
    }
}
