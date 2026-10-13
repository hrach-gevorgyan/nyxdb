//! Attachment bytes as plain files: one file per distinct content, named
//! by its sha256 in hex, in one folder per database
//! (`<data>/att/<db>/<sha256>`). sled keeps only the md5 → sha256 index.
//!
//! Before v0.1.8 the bytes lived in sled, which (a) used ~2.6× their size
//! on disk and never gave it back, (b) kept them in memory: its cache cap
//! doesn't bound large values, so resident memory grew with every
//! attachment byte until a restart, and (c) left a deleted database's
//! bytes readable in its files. See `doc/changelog.md`.

use std::collections::HashSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Prefix of a file being written; renamed to its sha256 once complete.
/// Followed by this process's boot id, so temp files left by a process that
/// died can be told apart from uploads in progress (`remove_stale_temps`).
const TEMP_PREFIX: &str = ".tmp-";

fn boot_id() -> &'static str {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
}

#[derive(Clone, Debug)]
pub struct BlobStore {
    dir: PathBuf,
}

/// What a sweep removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    pub files: usize,
    pub bytes: u64,
    pub temp_files: usize,
}

impl BlobStore {
    /// The folder for database `db_name` under `att_root`. Database names
    /// may contain `/` (CouchDB allows it); it becomes `%2F`, which can't
    /// clash because `%` is not allowed in database names.
    pub fn for_db(att_root: &Path, db_name: &str) -> Self {
        Self { dir: att_root.join(db_name.replace('/', "%2F")) }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn path(&self, hex: &str) -> PathBuf {
        self.dir.join(hex)
    }

    pub fn exists(&self, hex: &str) -> bool {
        valid_hex(hex) && self.path(hex).is_file()
    }

    pub fn get(&self, hex: &str) -> io::Result<Option<Vec<u8>>> {
        if !valid_hex(hex) {
            return Ok(None);
        }
        match fs::read(self.path(hex)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Stores `bytes` as `<hex>`, durably, before returning: written to a
    /// temp file, fsynced, renamed into place, and the folder fsynced so the
    /// rename itself survives a crash. A file with the final name is
    /// therefore always complete. If the content is already stored, its
    /// modification time is refreshed instead, so a sweep's age rule
    /// protects it while the revision that references it is committed.
    pub fn put(&self, hex: &str, bytes: &[u8]) -> io::Result<()> {
        if !valid_hex(hex) {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("not a sha256 hex digest: {hex:?}")));
        }
        let path = self.path(hex);
        if path.is_file() {
            return touch(&path);
        }
        fs::create_dir_all(&self.dir)?;
        let temp = self.dir.join(format!("{TEMP_PREFIX}{}-{}", boot_id(), uuid::Uuid::new_v4().simple()));
        let written = (|| {
            let mut file = fs::File::create(&temp)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temp, &path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            // Another writer may have stored the same content meanwhile.
            return if path.is_file() { touch(&path) } else { Err(e) };
        }
        sync_dir(&self.dir)
    }

    /// Deletes content files not in `referenced` and leftover temp files,
    /// but only those last modified more than `grace` ago: a file that new
    /// may belong to an upload whose revision isn't committed yet.
    pub fn sweep(&self, referenced: &HashSet<String>, grace: Duration) -> io::Result<SweepStats> {
        let mut stats = SweepStats::default();
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(stats),
            Err(e) => return Err(e),
        };
        let now = SystemTime::now();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_temp = name.starts_with(TEMP_PREFIX);
            if !is_temp && (!valid_hex(&name) || referenced.contains(&name)) {
                continue;
            }
            let meta = entry.metadata()?;
            let age = meta.modified().ok().and_then(|m| now.duration_since(m).ok()).unwrap_or_default();
            if age < grace {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) if is_temp => stats.temp_files += 1,
                Ok(()) => {
                    stats.files += 1;
                    stats.bytes += meta.len();
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        if stats.files + stats.temp_files > 0 {
            sync_dir(&self.dir)?;
        }
        Ok(stats)
    }

    /// Deletes temp files written by an earlier process — one killed
    /// mid-write. Those can't be in progress, so they go at once instead of
    /// waiting out a sweep's grace period. Returns how many.
    pub fn remove_stale_temps(&self) -> io::Result<usize> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let ours = format!("{TEMP_PREFIX}{}-", boot_id());
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(TEMP_PREFIX) && !name.starts_with(&ours) {
                match fs::remove_file(entry.path()) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(removed)
    }

    /// Deletes the whole folder (`DELETE /{db}`).
    pub fn remove_all(&self) -> io::Result<()> {
        match fs::remove_dir_all(&self.dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    }
}

fn valid_hex(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn touch(path: &Path) -> io::Result<()> {
    fs::File::options().write(true).open(path)?.set_modified(SystemTime::now())
}

/// Makes a rename or delete in `dir` durable. On Windows a directory can
/// be opened (FILE_FLAG_BACKUP_SEMANTICS) and flushed like a file.
fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        fs::OpenOptions::new().write(true).custom_flags(FILE_FLAG_BACKUP_SEMANTICS).open(dir)?.sync_all()
    }
    #[cfg(not(windows))]
    {
        fs::File::open(dir)?.sync_all()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store(tag: &str) -> BlobStore {
        let root = std::env::temp_dir().join(format!("nyxdb-blobs-{tag}-{}", uuid::Uuid::new_v4()));
        BlobStore::for_db(&root, "db/with/slashes")
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn put_get_and_dedup() {
        let store = temp_store("put");
        store.put(A, b"hello").unwrap();
        store.put(A, b"hello").unwrap();
        assert_eq!(store.get(A).unwrap().as_deref(), Some(&b"hello"[..]));
        assert_eq!(store.get(B).unwrap(), None);
        assert_eq!(fs::read_dir(store.dir()).unwrap().count(), 1, "one file, no temp left");
        assert!(store.dir().ends_with("db%2Fwith%2Fslashes"));
        store.remove_all().unwrap();
        assert!(!store.dir().exists());
    }

    #[test]
    fn sweep_spares_referenced_and_recent_files() {
        let store = temp_store("sweep");
        store.put(A, b"kept").unwrap();
        store.put(B, b"unreferenced").unwrap();
        fs::write(store.dir().join(format!("{TEMP_PREFIX}leftover")), b"partial").unwrap();
        let referenced: HashSet<String> = [A.to_string()].into();

        let none = store.sweep(&referenced, Duration::from_secs(3600)).unwrap();
        assert_eq!(none, SweepStats::default(), "nothing older than the grace period");
        assert!(store.exists(B));

        let swept = store.sweep(&referenced, Duration::ZERO).unwrap();
        assert_eq!(swept, SweepStats { files: 1, bytes: 12, temp_files: 1 });
        assert!(store.exists(A) && !store.exists(B));
        store.remove_all().unwrap();
    }

    #[test]
    fn temps_of_a_dead_process_go_at_once_ours_stay() {
        let store = temp_store("stale");
        store.put(A, b"x").unwrap();
        let dead = store.dir().join(format!("{TEMP_PREFIX}0123456789abcdef-killed"));
        let ours = store.dir().join(format!("{TEMP_PREFIX}{}-uploading", boot_id()));
        fs::write(&dead, b"half").unwrap();
        fs::write(&ours, b"half").unwrap();
        assert_eq!(store.remove_stale_temps().unwrap(), 1);
        assert!(!dead.exists() && ours.exists() && store.exists(A));
        store.remove_all().unwrap();
    }
}
