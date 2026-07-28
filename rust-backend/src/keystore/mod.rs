//! Master key storage.
//!
//! The 256-bit master key is written to a file in the user's local app data
//! directory. It is NOT wrapped by the OS keychain and NOT protected by a
//! passphrase: anything running as this user, and anyone who can read the
//! user's profile off the disk, can read the key and therefore open every
//! SQLCipher database. On Unix the file is created with mode 0600, which keeps
//! *other* local users out — that is the whole of the protection. Full-disk
//! encryption remains the user's responsibility, and README.md says so.
//!
//! Moving this to the platform keychain (Keychain Services / DPAPI / Secret
//! Service) is open work; the `prompt` argument is the seam it will hook into,
//! which is why it is threaded through unused.

use std::path::PathBuf;

#[derive(Debug)]
pub enum KeyStoreError {
    NotFound,
    Other(String),
}

pub async fn load_master_key(_prompt: &str) -> Result<Vec<u8>, KeyStoreError> {
    let path = get_key_path();
    match tokio::fs::read(&path).await {
        Ok(data) => Ok(data),
        Err(_) => Err(KeyStoreError::NotFound),
    }
}

pub async fn store_master_key(_prompt: &str, key: &[u8]) -> Result<(), KeyStoreError> {
    let path = get_key_path();
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    write_private(&path, key)
        .await
        .map_err(|e| KeyStoreError::Other(e.to_string()))
}

/// Write the key so only the owning user can read it. On Unix the mode is set
/// in the `open` call itself, so the key is never briefly world-readable.
#[cfg(unix)]
async fn write_private(path: &PathBuf, key: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::io::AsyncWriteExt;

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await?;
    file.write_all(key).await?;
    file.sync_all().await?;

    // A key file left by an earlier build may still carry loose permissions;
    // tighten it on every write.
    let mut perms = tokio::fs::metadata(path).await?.permissions();
    perms.set_mode(0o600);
    tokio::fs::set_permissions(path, perms).await
}

#[cfg(not(unix))]
async fn write_private(path: &PathBuf, key: &[u8]) -> std::io::Result<()> {
    // Windows: the local app data directory is already per-user ACL'd and the
    // file inherits that. There is no mode to set here.
    tokio::fs::write(path, key).await
}

fn get_key_path() -> PathBuf {
    let mut path = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    path.push("com.guvercin.app");
    path.push("master.key");
    path
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn written_key_is_owner_only() {
        let dir = std::env::temp_dir().join(format!("guvercin-keystore-{}", std::process::id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let path = dir.join("master.key");

        // Pre-create it world-readable to prove the mode gets tightened.
        tokio::fs::write(&path, b"old").await.unwrap();
        let mut loose = tokio::fs::metadata(&path).await.unwrap().permissions();
        loose.set_mode(0o644);
        tokio::fs::set_permissions(&path, loose).await.unwrap();

        write_private(&path, b"0123456789abcdef").await.unwrap();

        let mode = tokio::fs::metadata(&path).await.unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "master key must not be group/world readable");
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"0123456789abcdef");

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
