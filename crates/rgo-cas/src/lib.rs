//! Immutable filesystem-backed content-addressed storage for compiler outputs.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
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
}

impl Store {
    pub fn new(root: impl Into<PathBuf>, quarantine: impl Into<PathBuf>) -> Result<Self> {
        let store = Self {
            root: root.into(),
            quarantine: quarantine.into(),
        };
        fs::create_dir_all(store.objects_dir())?;
        fs::create_dir_all(store.manifests_dir())?;
        fs::create_dir_all(&store.quarantine)?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
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
        let temp = self.temp_path("object");
        {
            let mut file = File::create(&temp)?;
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
        let path = self.manifest_path(&manifest.key);
        let temp = self.temp_path("manifest");
        let bytes = serde_json::to_vec_pretty(manifest)?;
        {
            let mut file = File::create(&temp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        match fs::rename(&temp, &path) {
            Ok(()) => Ok(()),
            Err(_error) if path.is_file() => {
                let _ = fs::remove_file(temp);
                Ok(())
            }
            Err(error) => {
                let _ = fs::remove_file(temp);
                Err(error.into())
            }
        }
    }

    pub fn read_manifest(&self, key: &str) -> Result<Option<Manifest>> {
        let path = self.manifest_path(key);
        if !path.is_file() {
            return Ok(None);
        }
        let manifest: Manifest = serde_json::from_slice(&fs::read(&path)?)
            .with_context(|| format!("parsing {}", path.display()))?;
        if manifest.version != MANIFEST_VERSION || manifest.key != key {
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
                let _ = fs::remove_file(&path);
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
            match serde_json::from_slice::<Manifest>(&fs::read(&path)?) {
                Ok(manifest) if manifest.version == MANIFEST_VERSION => result.push(manifest),
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
            key: "abc".into(),
            outputs: vec![ManifestOutput {
                kind: "link".into(),
                name: "libdemo.rlib".into(),
                object: object.clone(),
            }],
            stdout: None,
            stderr: None,
            created_at: 1,
        };
        store.write_manifest(&manifest).unwrap();
        assert_eq!(store.read_manifest("abc").unwrap(), Some(manifest));
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
}
