//! Master key storage in the OS credential store, with an upgrade path for
//! installations that previously used an app-data key file.

use std::path::{Path, PathBuf};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

#[derive(Debug)]
pub enum KeyStoreError {
    NotFound,
    Other(String),
}

const MASTER_KEY_LEN: usize = 32;

fn entry() -> Result<keyring::Entry, KeyStoreError> {
    keyring::Entry::new("com.guvercin.app", "database-master-key").map_err(keyring_error)
}

fn keyring_error(error: keyring::Error) -> KeyStoreError {
    match error {
        keyring::Error::NoEntry => KeyStoreError::NotFound,
        other => KeyStoreError::Other(other.to_string()),
    }
}

fn validate_key(key: &[u8]) -> Result<(), KeyStoreError> {
    if key.len() == MASTER_KEY_LEN {
        Ok(())
    } else {
        Err(KeyStoreError::Other(
            "Master key has an invalid length.".into(),
        ))
    }
}

fn load_key(path: &Path, entry: &keyring::Entry) -> Result<Vec<u8>, KeyStoreError> {
    let legacy = match std::fs::read(path) {
        Ok(bytes) => {
            let bytes = Zeroizing::new(bytes);
            validate_key(&bytes)?;
            Some(bytes)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(KeyStoreError::Other(error.to_string())),
    };

    let saved = match entry.get_secret() {
        Ok(bytes) => Zeroizing::new(bytes),
        Err(keyring::Error::NoEntry) => {
            let Some(legacy) = &legacy else {
                return Err(KeyStoreError::NotFound);
            };
            entry.set_secret(legacy).map_err(keyring_error)?;
            Zeroizing::new(entry.get_secret().map_err(keyring_error)?)
        }
        Err(error) => return Err(keyring_error(error)),
    };
    validate_key(&saved)?;
    if let Some(legacy) = legacy {
        if !bool::from(saved.as_slice().ct_eq(legacy.as_slice())) {
            return Err(KeyStoreError::Other(
                "The legacy and platform master keys differ. The legacy file was preserved.".into(),
            ));
        }
        if let Err(error) = std::fs::remove_file(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(KeyStoreError::Other(error.to_string()));
            }
        }
    }
    Ok(saved.to_vec())
}

pub async fn load_master_key(_prompt: &str) -> Result<Vec<u8>, KeyStoreError> {
    tokio::task::spawn_blocking(|| load_key(&legacy_key_path(), &entry()?))
        .await
        .map_err(|error| KeyStoreError::Other(error.to_string()))?
}

pub async fn store_master_key(_prompt: &str, key: &[u8]) -> Result<(), KeyStoreError> {
    validate_key(key)?;
    let key = Zeroizing::new(key.to_vec());
    tokio::task::spawn_blocking(move || {
        let entry = entry()?;
        match entry.get_secret() {
            Ok(existing) => {
                let existing = Zeroizing::new(existing);
                if !bool::from(existing.as_slice().ct_eq(key.as_slice())) {
                    return Err(KeyStoreError::Other(
                        "A different master key already exists.".into(),
                    ));
                }
            }
            Err(keyring::Error::NoEntry) => entry.set_secret(&key).map_err(keyring_error)?,
            Err(error) => return Err(keyring_error(error)),
        }
        let verified = Zeroizing::new(entry.get_secret().map_err(keyring_error)?);
        if !bool::from(verified.as_slice().ct_eq(key.as_slice())) {
            return Err(KeyStoreError::Other(
                "Could not verify the stored master key.".into(),
            ));
        }
        Ok(())
    })
    .await
    .map_err(|error| KeyStoreError::Other(error.to_string()))?
}

fn legacy_key_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("com.guvercin.app")
        .join("master.key")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_legacy_key_and_preserves_the_file_if_keys_disagree() {
        let dir =
            std::env::temp_dir().join(format!("guvercin-keystore-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("master.key");
        let entry =
            keyring::Entry::new_with_credential(Box::new(keyring::mock::MockCredential::default()));
        let legacy = [7; MASTER_KEY_LEN];
        std::fs::write(&path, legacy).unwrap();

        entry.set_secret(&[8; MASTER_KEY_LEN]).unwrap();
        assert!(matches!(
            load_key(&path, &entry),
            Err(KeyStoreError::Other(_))
        ));
        assert!(path.exists());

        entry.delete_credential().unwrap();
        assert_eq!(load_key(&path, &entry).unwrap(), legacy);
        assert!(!path.exists());
        assert_eq!(entry.get_secret().unwrap(), legacy);
        std::fs::remove_dir(dir).unwrap();
    }
}
