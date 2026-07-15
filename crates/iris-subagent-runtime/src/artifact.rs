use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use rand::random;
use sha2::{Digest, Sha256};

use crate::{ArtifactId, ArtifactRef, RuntimeError};

/// Host-neutral content-addressed storage for oversized output and artifacts.
pub trait ArtifactStore: Send + Sync + 'static {
    /// Stores complete content and returns a stable reference.
    fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<ArtifactRef, RuntimeError>;

    /// Loads complete content by reference.
    fn get(&self, id: &ArtifactId) -> Result<Vec<u8>, RuntimeError>;
}

/// Filesystem implementation suitable for standalone runtime consumers.
#[derive(Debug, Clone)]
pub struct FilesystemArtifactStore {
    root: PathBuf,
}

impl FilesystemArtifactStore {
    /// Opens a content-addressed store at `root`.
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, RuntimeError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|source| RuntimeError::persistence(&root, source))?;
        Ok(Self { root })
    }

    fn path(&self, id: &ArtifactId) -> PathBuf {
        self.root.join(id.as_str())
    }
}

impl ArtifactStore for FilesystemArtifactStore {
    fn put(&self, bytes: &[u8], media_type: Option<&str>) -> Result<ArtifactRef, RuntimeError> {
        let digest = Sha256::digest(bytes);
        let id = ArtifactId::parse(format!("art_{}", hex(&digest[..16])))?;
        let path = self.path(&id);
        if !path.exists() {
            let temp = self.root.join(format!(".tmp-{:032x}", random::<u128>()));
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .map_err(|source| RuntimeError::persistence(&temp, source))?;
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|source| RuntimeError::persistence(&temp, source))?;
            match fs::hard_link(&temp, &path) {
                Ok(()) => {
                    fs::remove_file(&temp)
                        .map_err(|source| RuntimeError::persistence(&temp, source))?;
                    let directory = OpenOptions::new()
                        .read(true)
                        .open(&self.root)
                        .map_err(|source| RuntimeError::persistence(&self.root, source))?;
                    directory
                        .sync_all()
                        .map_err(|source| RuntimeError::persistence(&self.root, source))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = fs::remove_file(&temp);
                    if fs::read(&path).map_err(|source| RuntimeError::persistence(&path, source))?
                        != bytes
                    {
                        return Err(RuntimeError::Artifact(
                            "content digest collision in artifact store".to_string(),
                        ));
                    }
                }
                Err(source) => {
                    let _ = fs::remove_file(&temp);
                    return Err(RuntimeError::persistence(&path, source));
                }
            }
        } else if fs::read(&path).map_err(|source| RuntimeError::persistence(&path, source))?
            != bytes
        {
            return Err(RuntimeError::Artifact(
                "content digest collision in artifact store".to_string(),
            ));
        }
        Ok(ArtifactRef {
            id,
            bytes: bytes.len(),
            media_type: media_type.map(str::to_string),
        })
    }

    fn get(&self, id: &ArtifactId) -> Result<Vec<u8>, RuntimeError> {
        let path = self.path(id);
        let bytes = fs::read(&path).map_err(|source| RuntimeError::persistence(&path, source))?;
        let digest = Sha256::digest(&bytes);
        let expected = format!("art_{}", hex(&digest[..16]));
        if expected != id.as_str() {
            return Err(RuntimeError::CorruptRecord {
                path,
                message: "artifact digest does not match its ID".to_string(),
            });
        }
        Ok(bytes)
    }
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filesystem_store_is_content_addressed_and_round_trips() {
        let root = std::env::temp_dir().join(format!("iris-artifacts-{:032x}", random::<u128>()));
        let store = FilesystemArtifactStore::new(&root).unwrap();
        let one = store.put(b"complete output", Some("text/plain")).unwrap();
        let two = store.put(b"complete output", Some("text/plain")).unwrap();
        assert_eq!(one.id, two.id);
        assert_eq!(store.get(&one.id).unwrap(), b"complete output");
        fs::write(store.path(&one.id), b"forged").unwrap();
        assert!(
            store
                .put(b"complete output", Some("text/plain"))
                .unwrap_err()
                .to_string()
                .contains("collision")
        );
        fs::remove_dir_all(root).unwrap();
    }
}
