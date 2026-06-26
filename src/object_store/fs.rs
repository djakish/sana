//! Filesystem object store. Mutating operations are serialized by a single
//! in-process lock and made crash-safe with temp-file + atomic-rename. This is
//! correct for single-process local development; true cross-process CAS will
//! come from S3/GCS conditional writes. See `docs/PROGRESS.md` decision log.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock, Weak};

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use super::{GetResult, ObjectMeta, ObjectStore, ObjectVersion, version_of};
use crate::error::{Error, Result};

pub struct FsObjectStore {
    root: PathBuf,
    write_lock: Arc<tokio::sync::Mutex<()>>,
}

impl FsObjectStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = Self::normalize_root(root.into());
        let write_lock = Self::lock_for_root(&root);
        Self { root, write_lock }
    }

    fn normalize_root(root: PathBuf) -> PathBuf {
        match std::fs::canonicalize(&root) {
            Ok(root) => root,
            Err(_) if root.is_absolute() => root,
            Err(_) => std::env::current_dir()
                .map(|cwd| cwd.join(&root))
                .unwrap_or(root),
        }
    }

    fn lock_for_root(root: &Path) -> Arc<tokio::sync::Mutex<()>> {
        static WRITE_LOCKS: OnceLock<
            std::sync::Mutex<BTreeMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>,
        > = OnceLock::new();
        let locks = WRITE_LOCKS.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()));
        let mut locks = locks
            .lock()
            .expect("filesystem object-store lock registry is not poisoned");
        if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
        lock
    }

    /// Map an object key to a path under root, rejecting absolute paths and
    /// `..` traversal so a key can never escape the store root.
    fn resolve(&self, key: &str) -> Result<PathBuf> {
        if key.is_empty() {
            return Err(Error::Corrupt("empty object key".into()));
        }
        let rel = Path::new(key);
        for c in rel.components() {
            if !matches!(c, Component::Normal(_)) {
                return Err(Error::Corrupt(format!("invalid object key: {key}")));
            }
        }
        Ok(self.root.join(rel))
    }

    async fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".__tmp");
        let tmp = PathBuf::from(tmp);

        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(bytes).await?;
        f.sync_all().await?;
        drop(f);

        tokio::fs::rename(&tmp, path).await?;
        #[cfg(unix)]
        {
            let parent = path
                .parent()
                .ok_or_else(|| Error::Corrupt("object path has no parent".into()))?
                .to_owned();
            tokio::task::spawn_blocking(move || std::fs::File::open(parent)?.sync_all())
                .await
                .map_err(|e| Error::Corrupt(format!("directory sync join error: {e}")))??;
        }
        Ok(())
    }
}

#[async_trait]
impl ObjectStore for FsObjectStore {
    async fn get(&self, key: &str) -> Result<GetResult> {
        let path = self.resolve(key)?;
        match tokio::fs::read(&path).await {
            Ok(data) => {
                let version = version_of(&data);
                Ok(GetResult {
                    bytes: Bytes::from(data),
                    version,
                })
            }
            Err(e) if e.kind() == ErrorKind::NotFound => Err(Error::NotFound(key.to_string())),
            Err(e) => Err(Error::Io(e)),
        }
    }

    async fn get_range(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        let path = self.resolve(key)?;
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(Error::NotFound(key.to_string()));
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let size = file.metadata().await?.len();
        if range.start > range.end || range.end > size {
            return Err(Error::InvalidRange {
                start: range.start,
                end: range.end,
                size,
            });
        }
        let len = (range.end - range.start) as usize;
        file.seek(std::io::SeekFrom::Start(range.start)).await?;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).await?;
        Ok(Bytes::from(buf))
    }

    async fn put(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        let path = self.resolve(key)?;
        let _g = self.write_lock.lock().await;
        Self::write_atomic(&path, &bytes).await?;
        Ok(version_of(&bytes))
    }

    async fn put_if_absent(&self, key: &str, bytes: Bytes) -> Result<ObjectVersion> {
        let path = self.resolve(key)?;
        let _g = self.write_lock.lock().await;
        if tokio::fs::try_exists(&path).await? {
            return Err(Error::AlreadyExists(key.to_string()));
        }
        Self::write_atomic(&path, &bytes).await?;
        Ok(version_of(&bytes))
    }

    async fn compare_and_set(
        &self,
        key: &str,
        expected: ObjectVersion,
        bytes: Bytes,
    ) -> Result<ObjectVersion> {
        let path = self.resolve(key)?;
        let _g = self.write_lock.lock().await;
        let actual = match tokio::fs::read(&path).await {
            Ok(data) => Some(version_of(&data)),
            Err(e) if e.kind() == ErrorKind::NotFound => None,
            Err(e) => return Err(Error::Io(e)),
        };
        if actual.as_ref() != Some(&expected) {
            return Err(Error::CasMismatch {
                key: key.to_string(),
                expected,
                actual,
            });
        }
        Self::write_atomic(&path, &bytes).await?;
        Ok(version_of(&bytes))
    }

    async fn list(&self, prefix: &str) -> Result<Vec<ObjectMeta>> {
        let root = self.root.clone();
        let prefix = prefix.to_string();
        let metas = tokio::task::spawn_blocking(move || -> Result<Vec<ObjectMeta>> {
            let mut out = Vec::new();
            let mut stack = vec![root.clone()];
            while let Some(dir) = stack.pop() {
                let rd = match std::fs::read_dir(&dir) {
                    Ok(rd) => rd,
                    Err(e) if e.kind() == ErrorKind::NotFound => continue,
                    Err(e) => return Err(Error::Io(e)),
                };
                for entry in rd {
                    let entry = entry?;
                    let path = entry.path();
                    let ft = entry.file_type()?;
                    if ft.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if !ft.is_file() {
                        continue;
                    }
                    let rel = path.strip_prefix(&root).expect("entry is under root");
                    let key = rel
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/");
                    if key.ends_with(".__tmp") || !key.starts_with(&prefix) {
                        continue;
                    }
                    let data = std::fs::read(&path)?;
                    out.push(ObjectMeta {
                        key,
                        size: data.len() as u64,
                        version: version_of(&data),
                    });
                }
            }
            out.sort_by(|a, b| a.key.cmp(&b.key));
            Ok(out)
        })
        .await
        .map_err(|e| Error::Corrupt(format!("list join error: {e}")))??;
        Ok(metas)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.resolve(key)?;
        let _g = self.write_lock.lock().await;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(e) => Err(Error::Io(e)),
        }
    }
}
