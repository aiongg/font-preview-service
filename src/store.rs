//! The content-addressed layout of the data volume.
//!
//! ```text
//! <root>/files/<first 2 hex of sha256>/<sha256>      upstream font files
//! <root>/licences/<first 2 hex of sha256>/<sha256>   licence texts
//! <root>/manifest.json                               last fully successful sync
//! <root>/manifest.failed.json                        report of the last sync with failures
//! ```

use craft_font_store::sha256_hex;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    File,
    Licence,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::File => "files",
            Kind::Licence => "licences",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    pub fn failed_manifest_path(&self) -> PathBuf {
        self.root.join("manifest.failed.json")
    }

    /// Deletes `.*.tmp` files that interrupted `put` calls left in the blob directories.
    pub fn remove_stale_tmp(&self) {
        for kind in [Kind::File, Kind::Licence] {
            let Ok(subdirs) = fs::read_dir(self.root.join(kind.dir())) else {
                continue;
            };
            for sub in subdirs.flatten() {
                let Ok(entries) = fs::read_dir(sub.path()) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    if name.starts_with('.') && name.ends_with(".tmp") {
                        let _ = fs::remove_file(entry.path());
                    }
                }
            }
        }
    }

    /// Where the blob with this sha256 lives. `sha256` must be 64 lowercase hex digits.
    pub fn path(&self, kind: Kind, sha256: &str) -> PathBuf {
        self.root.join(kind.dir()).join(&sha256[..2]).join(sha256)
    }

    /// Whether the blob is on disk with exactly this size and sha256.
    pub fn has(&self, kind: Kind, sha256: &str, size: u64) -> bool {
        let path = self.path(kind, sha256);
        match fs::metadata(&path) {
            Ok(m) if m.is_file() && m.len() == size => {}
            _ => return false,
        }
        fs::read(&path).is_ok_and(|bytes| sha256_hex(&bytes) == sha256)
    }

    /// Writes the blob to a temporary name in its directory, then renames it into place.
    pub fn put(&self, kind: Kind, sha256: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(kind, sha256);
        let dir = path
            .parent()
            .ok_or_else(|| io::Error::other("no parent directory"))?;
        fs::create_dir_all(dir)?;
        let tmp = dir.join(format!(".{sha256}.{}.tmp", std::process::id()));
        let result = write_synced(&tmp, bytes).and_then(|()| fs::rename(&tmp, &path));
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_uses_the_first_two_hex_digits() {
        let store = Store::new("/data");
        let sha = "ab".to_string() + &"0".repeat(62);
        assert_eq!(
            store.path(Kind::File, &sha),
            PathBuf::from(format!("/data/files/ab/{sha}"))
        );
        assert_eq!(
            store.path(Kind::Licence, &sha),
            PathBuf::from(format!("/data/licences/ab/{sha}"))
        );
        assert_eq!(store.manifest_path(), PathBuf::from("/data/manifest.json"));
    }

    #[test]
    fn has_requires_matching_size_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let bytes = b"hello font";
        let sha = sha256_hex(bytes);
        assert!(!store.has(Kind::File, &sha, bytes.len() as u64));
        store.put(Kind::File, &sha, bytes).unwrap();
        assert!(store.has(Kind::File, &sha, bytes.len() as u64));
        assert!(!store.has(Kind::File, &sha, 3), "wrong size");
        assert!(
            !store.has(Kind::Licence, &sha, bytes.len() as u64),
            "other kind"
        );
        std::fs::write(store.path(Kind::File, &sha), b"HELLO font").unwrap();
        assert!(
            !store.has(Kind::File, &sha, bytes.len() as u64),
            "corrupt content is refetched"
        );
    }

    #[test]
    fn put_leaves_no_temporary_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let sha = sha256_hex(b"x");
        store.put(Kind::Licence, &sha, b"x").unwrap();
        let names: Vec<_> = std::fs::read_dir(store.path(Kind::Licence, &sha).parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from(&sha)]);
    }
}
