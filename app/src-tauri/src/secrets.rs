//! Secrets live only in Windows Credential Manager (via `keyring`), never on
//! disk or in logs.

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::Mutex;

pub const SERVICE: &str = "SlopTweak";
/// Mock mode keeps its fake keys apart from the real ones.
pub const MOCK_SERVICE: &str = "SlopTweak-mock";
pub const VAST_API_KEY: &str = "vast_api_key";
pub const CIVITAI_TOKEN: &str = "civitai_token";

/// Per-instance launch secret. Kept so a relaunch can reattach; deleted with
/// the instance.
pub fn launch_secret_name(instance_id: u64) -> String {
    format!("launch_secret:{instance_id}")
}

#[derive(Debug, thiserror::Error)]
#[error("credential store error: {0}")]
pub struct SecretError(String);

pub trait SecretStore: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<String>, SecretError>;
    fn set(&self, name: &str, value: &str) -> Result<(), SecretError>;
    /// Succeeds if the entry doesn't exist.
    fn delete(&self, name: &str) -> Result<(), SecretError>;
}

pub struct KeyringStore {
    service: &'static str,
}

impl KeyringStore {
    pub fn new(service: &'static str) -> Self {
        Self { service }
    }

    fn entry(&self, name: &str) -> Result<keyring::Entry, SecretError> {
        keyring::Entry::new(self.service, name).map_err(|e| SecretError(e.to_string()))
    }
}

impl SecretStore for KeyringStore {
    fn get(&self, name: &str) -> Result<Option<String>, SecretError> {
        match self.entry(name)?.get_password() {
            Ok(v) => Ok(Some(v)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(SecretError(e.to_string())),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretError> {
        self.entry(name)?
            .set_password(value)
            .map_err(|e| SecretError(e.to_string()))
    }

    fn delete(&self, name: &str) -> Result<(), SecretError> {
        match self.entry(name)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(SecretError(e.to_string())),
        }
    }
}

#[cfg(test)]
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>);

#[cfg(test)]
impl SecretStore for MemoryStore {
    fn get(&self, name: &str) -> Result<Option<String>, SecretError> {
        Ok(self.0.lock().unwrap().get(name).cloned())
    }

    fn set(&self, name: &str, value: &str) -> Result<(), SecretError> {
        self.0.lock().unwrap().insert(name.into(), value.into());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), SecretError> {
        self.0.lock().unwrap().remove(name);
        Ok(())
    }
}

/// Dev builds only: copy `VAST_API_KEY` / `CIVITAI_TOKEN` from the process
/// env or `HKCU\Environment` into the credential store if it's empty there.
/// Returns the names imported. Values are never logged.
#[cfg(debug_assertions)]
pub fn dev_import_from_env(store: &dyn SecretStore) -> Vec<&'static str> {
    let mut imported = Vec::new();
    for (env_name, secret_name) in [
        ("VAST_API_KEY", VAST_API_KEY),
        ("CIVITAI_TOKEN", CIVITAI_TOKEN),
    ] {
        if matches!(store.get(secret_name), Ok(Some(_))) {
            continue;
        }
        if let Some(v) = read_user_env(env_name) {
            if store.set(secret_name, &v).is_ok() {
                imported.push(secret_name);
            }
        }
    }
    imported
}

#[cfg(debug_assertions)]
fn read_user_env(name: &str) -> Option<String> {
    if let Ok(v) = std::env::var(name) {
        if !v.trim().is_empty() {
            return Some(v.trim().to_string());
        }
    }
    #[cfg(windows)]
    {
        let key = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey("Environment")
            .ok()?;
        let v: String = key.get_value(name).ok()?;
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_store_roundtrip() {
        let s = MemoryStore::default();
        assert_eq!(s.get("a").unwrap(), None);
        s.set("a", "1").unwrap();
        assert_eq!(s.get("a").unwrap().as_deref(), Some("1"));
        s.delete("a").unwrap();
        s.delete("a").unwrap();
        assert_eq!(s.get("a").unwrap(), None);
    }

    #[test]
    fn launch_secret_names_are_per_instance() {
        assert_ne!(launch_secret_name(1), launch_secret_name(2));
    }
}
