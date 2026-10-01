//! Immutable filesystem-backed content-addressed storage for compiler outputs.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};

pub const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub digest: String,
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestOutput {
    pub kind: String,
    pub name: String,
    pub object: ObjectRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub key: String,
    pub outputs: Vec<ManifestOutput>,
    pub stdout: Option<ObjectRef>,
    pub stderr: Option<ObjectRef>,
    pub created_at: u64,
}

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    quarantine: PathBuf,
    manifest_revision: Arc<AtomicU64>,
    manifest_mutation: Arc<Mutex<()>>,
}

/// A verified, synced manifest waiting for its final atomic publication.
/// The staging lock remains held until publication or cancellation.
pub struct PreparedManifest {
    path: PathBuf,
    temp: PathBuf,
    manifest_revision: Arc<AtomicU64>,
    manifest_mutation: Arc<Mutex<()>>,
    _publication: File,
}

impl PreparedManifest {
    pub fn publish(self) -> Result<()> {
        let _mutation = self.manifest_mutation.lock().unwrap();
        match fs::rename(&self.temp, &self.path) {
            Ok(()) => {
                self.manifest_revision.fetch_add(1, Ordering::AcqRel);
                Ok(())
            }
            Err(error) => {
                // Some platforms cannot replace an existing file with rename.
                // Treat that as an idempotent publication only when its bytes
                // are exactly the staged manifest; otherwise the caller must
                // not index the new manifest as if it had been committed.
                let regular = fs::symlink_metadata(&self.path)
                    .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
                let identical = regular
                    && matches!(
                        (fs::read(&self.path), fs::read(&self.temp)),
                        (Ok(existing), Ok(staged)) if existing == staged
                    );
                if identical {
                    self.manifest_revision.fetch_add(1, Ordering::AcqRel);
                    Ok(())
                } else {
                    Err(error)
                        .with_context(|| format!("publishing CAS manifest {}", self.path.display()))
                }
            }
        }
    }
}

impl Drop for PreparedManifest {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.temp);
    }
}

impl Store {
    pub fn new(root: impl Into<PathBuf>, quarantine: impl Into<PathBuf>) -> Result<Self> {
        let store = Self {
            root: root.into(),
            quarantine: quarantine.into(),
            manifest_revision: Arc::new(AtomicU64::new(0)),
            manifest_mutation: Arc::new(Mutex::new(())),
        };
        fs::create_dir_all(&store.root)?;
        open_staging_lock(&store.root)?;
        fs::create_dir_all(store.objects_dir())?;
        fs::create_dir_all(store.manifests_dir())?;
        fs::create_dir_all(&store.quarantine)?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Changes made through this store while the daemon is running. Clone
    /// instances share the counter; other processes must not publish manifests.
    pub fn manifest_revision(&self) -> u64 {
        self.manifest_revision.load(Ordering::Acquire)
    }

    /// Apply an inventory only while its revision is still current. The
    /// callback must not publish or remove a manifest through this store.
    pub fn with_manifest_revision(
        &self,
        expected: u64,
        apply: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        let _mutation = self.manifest_mutation.lock().unwrap();
        if self.manifest_revision() != expected {
            return Ok(false);
        }
        apply()?;
        Ok(true)
    }

    pub fn object_path(&self, digest: &str) -> PathBuf {
        let shard = digest.get(..2).unwrap_or("00");
        self.objects_dir().join(shard).join(digest)
    }

    pub fn manifest_path(&self, key: &str) -> PathBuf {
        self.manifests_dir().join(format!("{key}.json"))
    }

    pub fn put_bytes(&self, bytes: &[u8], mode: u32) -> Result<ObjectRef> {
        let digest = blake3::hash(bytes).to_hex().to_string();
        let object = ObjectRef {
            digest: digest.clone(),
            size: bytes.len() as u64,
            mode,
        };
        let path = self.object_path(&digest);
        if path.is_file() && self.verify_object(&object).is_ok() {
            return Ok(object);
        }
        let parent = path.parent().context("object path has no parent")?;
        fs::create_dir_all(parent)?;
        let _publication = self.lock_staging_publication()?;
        let temp = self.temp_path("object");
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
        }
        let actual = digest_file(&temp)?;
        if actual != digest {
            let _ = fs::remove_file(&temp);
            bail!("CAS digest mismatch while publishing object")
        }
        match fs::rename(&temp, &path) {
            Ok(()) => set_read_only(&path)?,
            Err(error) if path.is_file() => {
                let _ = fs::remove_file(&temp);
                self.verify_object(&object)
                    .with_context(|| format!("concurrent CAS publication after {error}"))?;
            }
            Err(error) => {
                let _ = fs::remove_file(&temp);
                return Err(error.into());
            }
        }
        Ok(object)
    }

    pub fn put_file(&self, source: &Path, mode: u32) -> Result<ObjectRef> {
        let mut file =
            File::open(source).with_context(|| format!("opening {}", source.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.put_bytes(&bytes, mode)
    }

    pub fn read_object(&self, object: &ObjectRef) -> Result<Vec<u8>> {
        self.verify_object(object)?;
        Ok(fs::read(self.object_path(&object.digest))?)
    }

    pub fn verify_object(&self, object: &ObjectRef) -> Result<()> {
        if !valid_digest(&object.digest) {
            bail!("invalid CAS object digest")
        }
        let path = self.object_path(&object.digest);
        let metadata =
            fs::metadata(&path).with_context(|| format!("missing CAS object {}", object.digest))?;
        if metadata.len() != object.size || digest_file(&path)? != object.digest {
            self.quarantine(&path)?;
            bail!("CAS integrity check failed for {}", object.digest)
        }
        Ok(())
    }

    pub fn quarantine(&self, path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let name = path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("object");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let target = self.quarantine.join(format!("{name}.{stamp}.bad"));
        fs::rename(path, target)?;
        Ok(())
    }

    pub fn write_manifest(&self, manifest: &Manifest) -> Result<()> {
        self.prepare_manifest(manifest)?.publish()
    }

    pub fn prepare_manifest(&self, manifest: &Manifest) -> Result<PreparedManifest> {
        if !valid_key(&manifest.key) {
            bail!("invalid cache key")
        }
        if manifest.version != MANIFEST_VERSION {
            bail!("unsupported manifest version {}", manifest.version)
        }
        for output in &manifest.outputs {
            self.verify_object(&output.object)?;
        }
        if let Some(stdout) = &manifest.stdout {
            self.verify_object(stdout)?;
        }
        if let Some(stderr) = &manifest.stderr {
            self.verify_object(stderr)?;
        }
        let publication = self.lock_staging_publication()?;
        let prepared = PreparedManifest {
            path: self.manifest_path(&manifest.key),
            temp: self.temp_path("manifest"),
            manifest_revision: self.manifest_revision.clone(),
            manifest_mutation: self.manifest_mutation.clone(),
            _publication: publication,
        };
        let bytes = serde_json::to_vec_pretty(manifest)?;
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&prepared.temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        Ok(prepared)
    }

    pub fn read_manifest(&self, key: &str) -> Result<Option<Manifest>> {
        if !valid_key(key) {
            bail!("invalid cache key")
        }
        let path = self.manifest_path(key);
        if !path.is_file() {
            return Ok(None);
        }
        let bytes = fs::read(&path)?;
        let manifest: Manifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parsing {}", path.display()))?;
        if manifest.version != MANIFEST_VERSION
            || manifest.key != key
            || !valid_manifest_objects(&manifest)
        {
            bail!("invalid manifest for cache key {key}")
        }
        let objects = manifest
            .outputs
            .iter()
            .map(|output| &output.object)
            .chain(manifest.stdout.iter())
            .chain(manifest.stderr.iter());
        for object in objects {
            if let Err(error) = self.verify_object(object) {
                let _mutation = self.manifest_mutation.lock().unwrap();
                // A different publisher may have replaced this manifest while
                // object verification ran. Never remove its newer bytes.
                if fs::read(&path).ok().as_deref() == Some(bytes.as_slice())
                    && fs::remove_file(&path).is_ok()
                {
                    self.manifest_revision.fetch_add(1, Ordering::AcqRel);
                }
                return Err(error);
            }
        }
        Ok(Some(manifest))
    }

    pub fn list_manifests(&self) -> Result<Vec<Manifest>> {
        let mut result = Vec::new();
        for entry in fs::read_dir(self.manifests_dir())? {
            let path = entry?.path();
            if path.extension().and_then(|v| v.to_str()) != Some("json") {
                continue;
            }
            let bytes = match fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error).context("reading CAS manifest"),
            };
            match serde_json::from_slice::<Manifest>(&bytes) {
                Ok(manifest)
                    if manifest.version == MANIFEST_VERSION
                        && valid_key(&manifest.key)
                        && valid_manifest_objects(&manifest)
                        && path.file_name().and_then(|v| v.to_str())
                            == Some(format!("{}.json", manifest.key).as_str()) =>
                {
                    result.push(manifest)
                }
                Ok(_) | Err(_) => {}
            }
        }
        Ok(result)
    }

    pub fn object_bytes(&self) -> Result<u64> {
        Ok(sum_files(&self.objects_dir())?)
    }

    fn objects_dir(&self) -> PathBuf {
        self.root.join("objects")
    }
    fn manifests_dir(&self) -> PathBuf {
        self.root.join("manifests")
    }
    fn temp_path(&self, kind: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        self.root
            .join(format!(".{kind}.{stamp}.{}.tmp", std::process::id()))
    }

    fn lock_staging_publication(&self) -> Result<File> {
        let file = open_staging_lock(&self.root)?;
        FileExt::lock_shared(&file).context("locking CAS publication staging")?;
        Ok(file)
    }
}

/// GC takes this lock before considering or removing abandoned CAS staging
/// files. Every Store writer holds the shared side through its final rename.
/// A busy lock means a producer may still own a staging path.
pub fn try_lock_staging_cleanup(root: &Path) -> Result<Option<File>> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Ok(_) => bail!("unsafe CAS storage root {}", root.display()),
        Err(error) => return Err(error).context("checking CAS storage root"),
    }
    let file = open_staging_lock(root)?;
    if FileExt::try_lock_exclusive(&file).context("locking CAS staging cleanup")? {
        Ok(Some(file))
    } else {
        Ok(None)
    }
}

fn open_staging_lock(root: &Path) -> Result<File> {
    let root_metadata = fs::symlink_metadata(root)?;
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        bail!("unsafe CAS storage root {}", root.display());
    }
    let path = root.join(".staging.lock");
    match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => bail!("unsafe CAS staging lock {}", path.display()),
        Err(error) => return Err(error).context("checking CAS staging lock"),
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    if !file.metadata()?.is_file() {
        bail!("unsafe CAS staging lock {}", path.display());
    }
    Ok(file)
}

fn valid_key(key: &str) -> bool {
    key.len() == 64 && key.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_manifest_objects(manifest: &Manifest) -> bool {
    manifest
        .outputs
        .iter()
        .map(|output| &output.object)
        .chain(manifest.stdout.iter())
        .chain(manifest.stderr.iter())
        .all(|object| valid_digest(&object.digest))
}

fn digest_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn set_read_only(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions)?;
    Ok(())
}

fn sum_files(root: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    if !root.exists() {
        return Ok(0);
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = fs::metadata(&path)?;
        if metadata.is_dir() {
            total += sum_files(&path)?;
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publishes_and_reopens_immutable_objects() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("cas"), temp.path().join("quarantine")).unwrap();
        let object = store.put_bytes(b"hello", 0o644).unwrap();
        assert_eq!(store.read_object(&object).unwrap(), b"hello");
        assert!(
            fs::metadata(store.object_path(&object.digest))
                .unwrap()
                .permissions()
                .readonly()
        );
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            key: "a".repeat(64),
            outputs: vec![ManifestOutput {
                kind: "link".into(),
                name: "libdemo.rlib".into(),
                object: object.clone(),
            }],
            stdout: None,
            stderr: None,
            created_at: 1,
        };
        assert_eq!(store.manifest_revision(), 0);
        store.write_manifest(&manifest).unwrap();
        assert_eq!(store.clone().manifest_revision(), 1);
        assert_eq!(
            store.read_manifest(&"a".repeat(64)).unwrap(),
            Some(manifest)
        );
        let mut staged_manifest = store.read_manifest(&"a".repeat(64)).unwrap().unwrap();
        staged_manifest.key = "b".repeat(64);
        let prepared = store.prepare_manifest(&staged_manifest).unwrap();
        assert!(!store.manifest_path(&staged_manifest.key).exists());
        drop(prepared);
        assert!(!store.manifest_path(&staged_manifest.key).exists());
        assert_eq!(
            fs::read_dir(store.root())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".manifest."))
                .count(),
            0
        );
        store
            .prepare_manifest(&staged_manifest)
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(
            store.read_manifest(&staged_manifest.key).unwrap(),
            Some(staged_manifest.clone())
        );
        let mut replacement = store.read_manifest(&"a".repeat(64)).unwrap().unwrap();
        replacement.created_at = 2;
        match store.write_manifest(&replacement) {
            Ok(()) => {
                assert_eq!(
                    store.read_manifest(&replacement.key).unwrap(),
                    Some(replacement)
                );
            }
            Err(_) => {
                assert_eq!(
                    store
                        .read_manifest(&"a".repeat(64))
                        .unwrap()
                        .unwrap()
                        .created_at,
                    1
                );
            }
        }
        assert!(store.read_manifest("../../outside").is_err());
        let mut unsafe_manifest = store.read_manifest(&"a".repeat(64)).unwrap().unwrap();
        unsafe_manifest.key = "../../outside".into();
        assert!(store.write_manifest(&unsafe_manifest).is_err());
        let revision = store.manifest_revision();
        let object_path = store.object_path(&object.digest);
        #[cfg(windows)]
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut permissions = fs::metadata(&object_path).unwrap().permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&object_path, permissions).unwrap();
        }
        fs::remove_file(&object_path).unwrap();
        assert!(store.read_manifest(&staged_manifest.key).is_err());
        assert!(!store.manifest_path(&staged_manifest.key).exists());
        assert_eq!(store.manifest_revision(), revision + 1);
    }

    #[test]
    fn corrupt_objects_are_quarantined() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("cas"), temp.path().join("quarantine")).unwrap();
        let object = store.put_bytes(b"hello", 0o644).unwrap();
        let path = store.object_path(&object.digest);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        #[cfg(not(unix))]
        #[allow(clippy::permissions_set_readonly_false)]
        {
            let mut permissions = fs::metadata(&path).unwrap().permissions();
            permissions.set_readonly(false);
            fs::set_permissions(&path, permissions).unwrap();
        }
        fs::write(&path, b"bad").unwrap();
        assert!(store.verify_object(&object).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn malformed_manifest_object_digest_is_never_used_as_a_path() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path().join("cas"), temp.path().join("quarantine")).unwrap();
        let key = "a".repeat(64);
        let object = ObjectRef {
            digest: "../../outside".into(),
            size: 1,
            mode: 0o444,
        };
        let manifest = Manifest {
            version: MANIFEST_VERSION,
            key: key.clone(),
            outputs: vec![ManifestOutput {
                kind: "rlib".into(),
                name: "libdemo.rlib".into(),
                object: object.clone(),
            }],
            stdout: None,
            stderr: None,
            created_at: 1,
        };
        fs::write(
            store.manifest_path(&key),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(store.verify_object(&object).is_err());
        assert!(store.read_manifest(&key).is_err());
        assert!(store.list_manifests().unwrap().is_empty());
    }
}
