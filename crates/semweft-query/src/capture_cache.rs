//! Only watched refreshes use metadata reuse. Foreground latest queries always
//! verify content; periodic full reads recover missing native notifications.
use crate::{
    config::Resolved,
    workspace::{self, Capture},
};
use anyhow::{Context, Result};
use semweft_core::SourceFile;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime},
};

#[derive(PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(unix)]
    changed: (i64, i64),
}
impl Stamp {
    fn new(meta: &fs::Metadata) -> Self {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            #[cfg(unix)]
            device: meta.dev(),
            #[cfg(unix)]
            inode: meta.ino(),
            #[cfg(unix)]
            changed: (meta.ctime(), meta.ctime_nsec()),
        }
    }
}
#[derive(Default)]
pub struct CaptureCache {
    files: BTreeMap<PathBuf, (Stamp, SourceFile)>,
    seen: BTreeSet<PathBuf>,
    dirty: BTreeSet<PathBuf>,
    force: bool,
    last_full: Option<Instant>,
    pub files_read: usize,
    pub files_reused: usize,
}
impl CaptureCache {
    pub fn invalidate(&mut self, paths: &[PathBuf], full: bool) {
        self.force |= full;
        self.dirty.extend(paths.iter().cloned());
    }
    pub fn capture(&mut self, resolved: &Resolved, backend: &str, verify: bool) -> Result<Capture> {
        self.force |= verify
            || self
                .last_full
                .is_none_or(|last| last.elapsed() >= Duration::from_secs(5));
        self.seen.clear();
        self.files_read = 0;
        self.files_reused = 0;
        let result = workspace::capture_config_cached(resolved, backend, Some(self));
        if result.is_ok() {
            self.files.retain(|path, _| self.seen.contains(path));
            if self.force {
                self.last_full = Some(Instant::now());
            }
            self.force = false;
            self.dirty.clear();
        }
        result
    }
    pub(crate) fn read(&mut self, path: &Path) -> Result<SourceFile> {
        self.seen.insert(path.into());
        let stamp = Stamp::new(&fs::metadata(path)?);
        if !self.force
            && !self.dirty.contains(path)
            && let Some((old, file)) = self.files.get(path)
            && old == &stamp
        {
            self.files_reused += 1;
            return Ok(file.clone());
        }
        let text = fs::read_to_string(path)
            .with_context(|| format!("invalid_argument: input is not UTF-8: {}", path.display()))?;
        // Never associate bytes with a stamp from a different filesystem version.
        let after = Stamp::new(&fs::metadata(path)?);
        let file = SourceFile::new(text);
        if stamp == after {
            self.files.insert(path.into(), (stamp, file.clone()));
        } else {
            self.files.remove(path);
        }
        self.files_read += 1;
        Ok(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn incremental_capture_matches_full_capture_through_edits_renames_deletes_and_verification() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("src")).unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname=\"capture-test\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        fs::write(temp.path().join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let resolved = crate::config::resolve(Some(temp.path()), None).unwrap();
        let mut cache = CaptureCache::default();
        let first = cache.capture(&resolved, "none", false).unwrap();
        assert_eq!(cache.files_read, 2);
        assert_eq!(
            cache.capture(&resolved, "none", false).unwrap().manifest,
            first.manifest
        );
        assert_eq!((cache.files_read, cache.files_reused), (0, 2));
        let path = resolved.root.join("src/lib.rs");
        fs::write(&path, "pub fn b() {}\n").unwrap();
        assert_eq!(
            cache.capture(&resolved, "none", false).unwrap().manifest,
            workspace::capture_config(&resolved, "none")
                .unwrap()
                .manifest
        );
        assert_eq!(cache.files_read, 1);
        fs::rename(&path, resolved.root.join("src/new.rs")).unwrap();
        assert_eq!(
            cache.capture(&resolved, "none", false).unwrap().manifest,
            workspace::capture_config(&resolved, "none")
                .unwrap()
                .manifest
        );
        fs::remove_file(resolved.root.join("src/new.rs")).unwrap();
        assert_eq!(
            cache.capture(&resolved, "none", false).unwrap().manifest,
            workspace::capture_config(&resolved, "none")
                .unwrap()
                .manifest
        );
        assert_eq!(cache.files.len(), 1);
        cache.capture(&resolved, "none", true).unwrap();
        assert_eq!(cache.files_read, 1);
        cache.last_full = Some(Instant::now() - Duration::from_secs(6));
        cache.capture(&resolved, "none", false).unwrap();
        assert_eq!(cache.files_read, 1);
        fs::write(&path, [255]).unwrap();
        cache.invalidate(std::slice::from_ref(&path), false);
        assert!(cache.capture(&resolved, "none", false).is_err());
        fs::write(path, "pub fn recovered() {}\n").unwrap();
        assert_eq!(
            cache.capture(&resolved, "none", false).unwrap().manifest,
            workspace::capture_config(&resolved, "none")
                .unwrap()
                .manifest
        );
    }
}
