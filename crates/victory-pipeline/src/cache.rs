use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::Converter;

/// Identifies one conversion result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey(blake3::Hash);

impl CacheKey {
    pub fn new(converter: &dyn Converter, input: &[u8]) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(converter.id().as_bytes());
        h.update(&[0]);
        h.update(&converter.version().to_le_bytes());
        h.update(input);
        CacheKey(h.finalize())
    }

    pub fn to_hex(&self) -> String {
        self.0.to_hex().to_string()
    }
}

/// Content-addressed store under the user's cache directory.
#[derive(Debug, Clone)]
pub struct Cache {
    root: PathBuf,
}

impl Cache {
    /// Opens (creating) a cache at `root`. Refuses locations inside the repo.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self> {
        let root = root.into();
        victory_core::ensure_outside_repo(&root)?;
        std::fs::create_dir_all(&root).with_context(|| format!("creating {}", root.display()))?;
        Ok(Cache { root })
    }

    /// Opens (creating) generation `generation` of a cache at `root`: its
    /// entries are kept in `root/gen-<generation>`. A generation is what
    /// made the entries (a converter's own sources): one key includes it, so
    /// the entries of another generation can never be read again, and before
    /// generations nothing told them apart and nothing removed them. They go
    /// in [`Cache::sweep`]. `generation` is letters, digits, `-` and `_`.
    pub fn open_generation(root: impl Into<PathBuf>, generation: &str) -> Result<Self> {
        let named = !generation.is_empty()
            && generation.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        anyhow::ensure!(named, "a cache generation named {generation:?}");
        let cache = Cache::open(root.into().join(format!("{GENERATION}{generation}")))?;
        // Each run marks its generation used, so a sweep by a build of
        // another translator leaves it while it is still played with.
        let _ = std::fs::write(cache.root.join(USED), b"");
        Ok(cache)
    }

    /// For a cache of [`Cache::open_generation`]: removes the other
    /// generations beside it that no run has used for [`UNUSED_FOR`], and
    /// staging files in this generation that processes left as they ended.
    /// The layout from before generations (two hex digits as a folder) is
    /// left alone: older builds sharing the folder read it, and removing it
    /// made them translate every shader again (with another build's runs in
    /// between, at every start). Nothing of this generation that a process
    /// may still write is touched: a staging file is removed only when no
    /// process of its number runs.
    pub fn sweep(&self) -> Swept {
        let mut swept = Swept::default();
        let ours = self.root.file_name();
        // Only beside a generation: a cache of `Cache::open` has no others.
        let (Some(parent), true) = (
            self.root.parent(),
            ours.is_some_and(|n| n.to_string_lossy().starts_with(GENERATION)),
        ) else {
            return swept;
        };
        for entry in std::fs::read_dir(parent).into_iter().flatten().flatten() {
            let name = entry.file_name();
            let text = name.to_string_lossy();
            let old_generation = text.starts_with(GENERATION)
                && Some(name.as_os_str()) != ours
                && unused(&entry.path());
            if old_generation && entry.file_type().is_ok_and(|t| t.is_dir()) {
                let (files, bytes) = measure(&entry.path());
                if std::fs::remove_dir_all(entry.path()).is_ok() {
                    swept.generations += u64::from(old_generation);
                    swept.files += files;
                    swept.bytes += bytes;
                }
            }
        }
        for dir in std::fs::read_dir(&self.root).into_iter().flatten().flatten() {
            for file in std::fs::read_dir(dir.path()).into_iter().flatten().flatten() {
                let Some(pid) = staging_pid(&file.file_name().to_string_lossy()) else {
                    continue;
                };
                if pid == std::process::id() || process_runs(pid) {
                    continue;
                }
                let bytes = file.metadata().map_or(0, |m| m.len());
                if std::fs::remove_file(file.path()).is_ok() {
                    swept.files += 1;
                    swept.bytes += bytes;
                }
            }
        }
        swept
    }

    pub fn path_for(&self, key: &CacheKey) -> PathBuf {
        let hex = key.to_hex();
        self.root.join(&hex[..2]).join(&hex[2..])
    }

    /// Returns the cached output for `input`, converting it first if needed.
    pub fn get_or_convert(&self, converter: &dyn Converter, input: &[u8]) -> Result<PathBuf> {
        let key = CacheKey::new(converter, input);
        let path = self.path_for(&key);
        if path.is_file() {
            tracing::trace!(converter = converter.id(), key = %key.to_hex(), "cache hit");
            return Ok(path);
        }
        let output = converter
            .convert(input)
            .with_context(|| format!("converter {} v{}", converter.id(), converter.version()))?;
        let dir = path.parent().expect("hashed path has a parent");
        std::fs::create_dir_all(dir)?;
        // Write-then-rename so readers never see partial output.
        let (tmp, mut file) = tempfile_in(dir)?;
        let written = file.write_all(&output);
        drop(file);
        let published = written.and_then(|()| std::fs::rename(&tmp, &path));
        if published.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        published?;
        Ok(path)
    }
}

/// The start of a generation's folder name ([`Cache::open_generation`]).
const GENERATION: &str = "gen-";

/// The file a run of a generation writes on opening it ([`Cache::open_generation`]).
const USED: &str = "last-used";

/// How long a generation of another translator is kept unused.
const UNUSED_FOR: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// Whether no run has opened the generation in `dir` for [`UNUSED_FOR`]
/// (its mark, else the folder itself, from before marks).
fn unused(dir: &Path) -> bool {
    let at = std::fs::metadata(dir.join(USED))
        .or_else(|_| std::fs::metadata(dir))
        .and_then(|m| m.modified());
    at.ok()
        .and_then(|t| t.elapsed().ok())
        .is_some_and(|age| age >= UNUSED_FOR)
}

/// What [`Cache::sweep`] removed: generations, files and their bytes.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    pub generations: u64,
    pub files: u64,
    pub bytes: u64,
}

/// The files under `dir` and their bytes.
fn measure(dir: &Path) -> (u64, u64) {
    let mut out = (0, 0);
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                let (files, bytes) = measure(&entry.path());
                out = (out.0 + files, out.1 + bytes);
            }
            Ok(_) => out = (out.0 + 1, out.1 + entry.metadata().map_or(0, |m| m.len())),
            Err(_) => {}
        }
    }
    out
}

/// The process number in a staging file's name ([`create_tempfile_in`]).
fn staging_pid(name: &str) -> Option<u32> {
    let rest = name.strip_prefix(".tmp-")?;
    let (pid, n) = rest.split_once('-')?;
    n.parse::<u64>().ok()?;
    pid.parse().ok()
}

/// Whether a process numbered `pid` runs now (or may: when unsure, yes).
#[cfg(windows)]
fn process_runs(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, GetLastError, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    // SAFETY: plain queries on a handle we close.
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if process.is_null() {
            // No such process; anything else (a process we may not ask) runs.
            return GetLastError() != ERROR_INVALID_PARAMETER;
        }
        let mut code = 0u32;
        let asked = GetExitCodeProcess(process, &mut code) != 0;
        CloseHandle(process);
        !asked || code == STILL_ACTIVE as u32
    }
}

#[cfg(not(windows))]
fn process_runs(pid: u32) -> bool {
    !Path::new("/proc").is_dir() || Path::new(&format!("/proc/{pid}")).exists()
}

fn tempfile_in(dir: &Path) -> io::Result<(PathBuf, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    create_tempfile_in(dir, &COUNTER)
}

fn create_tempfile_in(dir: &Path, counter: &AtomicU64) -> io::Result<(PathBuf, File)> {
    // An interrupted writer can leave a file behind, and process IDs are
    // reused. Keep it intact and try a new name rather than failing a miss.
    for _ in 0..128 {
        let n = counter.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".tmp-{}-{n}", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => return Ok((path, file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique cache staging file after 128 attempts",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Upper {
        version: u32,
        runs: AtomicUsize,
    }

    impl Converter for Upper {
        fn id(&self) -> &'static str {
            "test.upper"
        }
        fn version(&self) -> u32 {
            self.version
        }
        fn convert(&self, input: &[u8]) -> Result<Vec<u8>> {
            self.runs.fetch_add(1, Ordering::Relaxed);
            Ok(input.to_ascii_uppercase())
        }
    }

    #[test]
    fn converts_once_then_hits() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let a = cache.get_or_convert(&conv, b"abc").unwrap();
        let b = cache.get_or_convert(&conv, b"abc").unwrap();
        assert_eq!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"ABC");
        assert_eq!(conv.runs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn failed_publication_removes_the_staging_file() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = Cache::open(tmp.path().join("cache")).unwrap();
        let conv = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let path = cache.path_for(&CacheKey::new(&conv, b"abc"));
        std::fs::create_dir_all(&path).unwrap();

        assert!(cache.get_or_convert(&conv, b"abc").is_err());
        assert!(path.is_dir());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
    }

    #[test]
    fn staging_retries_collisions_without_changing_existing_files() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        for n in 0..2 {
            std::fs::write(
                dir.path().join(format!(".tmp-{}-{n}", std::process::id())),
                b"stale",
            )
            .unwrap();
        }

        let (path, mut file) = create_tempfile_in(dir.path(), &counter).unwrap();
        file.write_all(b"fresh").unwrap();
        drop(file);
        assert_eq!(
            path.file_name().unwrap(),
            format!(".tmp-{}-2", std::process::id()).as_str()
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"fresh");
        for n in 0..2 {
            assert_eq!(
                std::fs::read(dir.path().join(format!(".tmp-{}-{n}", std::process::id()))).unwrap(),
                b"stale"
            );
        }
        assert_eq!(counter.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn staging_collision_retries_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        for n in 0..128 {
            std::fs::write(
                dir.path().join(format!(".tmp-{}-{n}", std::process::id())),
                b"stale",
            )
            .unwrap();
        }

        let error = create_tempfile_in(dir.path(), &counter).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(counter.load(Ordering::Relaxed), 128);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 128);
    }

    #[test]
    fn staging_does_not_retry_other_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        let counter = AtomicU64::new(0);
        let missing = dir.path().join("missing");

        let error = create_tempfile_in(&missing, &counter).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(counter.load(Ordering::Relaxed), 1);
        assert!(!missing.exists());
    }

    /// The number of a process that has ended.
    fn ended_pid() -> u32 {
        let mut child = if cfg!(windows) {
            std::process::Command::new("cmd").args(["/c", "exit"]).spawn()
        } else {
            std::process::Command::new("true").spawn()
        }
        .unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    /// Opening a generation keeps its entries; the sweep removes the other
    /// generations, the layout from before generations and the staging
    /// files of ended processes, and nothing else.
    #[test]
    fn a_sweep_removes_only_what_cannot_be_read_again() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("shaders");
        let conv = Upper { version: 1, runs: AtomicUsize::new(0) };
        let old = Cache::open_generation(&root, "old").unwrap();
        old.get_or_convert(&conv, b"old entry").unwrap();
        // The layout before generations, and a file that is no cache's.
        let flat = root.join("ab");
        std::fs::create_dir_all(&flat).unwrap();
        std::fs::write(flat.join("cdef"), b"an entry of an older build").unwrap();
        std::fs::write(root.join("notes.txt"), b"kept").unwrap();
        let cache = Cache::open_generation(&root, "new").unwrap();
        let kept = cache.get_or_convert(&conv, b"current entry").unwrap();
        let dir = kept.parent().unwrap().to_path_buf();
        let dead = dir.join(format!(".tmp-{}-0", ended_pid()));
        let own = dir.join(format!(".tmp-{}-0", std::process::id()));
        std::fs::write(&dead, b"left by a crash").unwrap();
        std::fs::write(&own, b"being written").unwrap();
        // A generation used lately stays (another build may still play with it).
        let first = cache.sweep();
        assert_eq!(first.generations, 0, "{first:?}");
        assert!(root.join("gen-old").exists());
        assert!(!dead.exists(), "a dead staging file goes at once");
        // Unused for longer than UNUSED_FOR: it goes.
        let long_ago = std::time::SystemTime::now() - UNUSED_FOR - std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(root.join("gen-old").join(USED))
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        let swept = cache.sweep();
        assert_eq!(swept.generations, 1);
        // The old entry and its mark.
        assert_eq!(swept.files, 2, "{swept:?}");
        assert!(swept.bytes > 0);
        assert!(!root.join("gen-old").exists());
        // The layout from before generations is older builds' cache: kept.
        assert!(flat.join("cdef").exists());
        assert!(own.exists(), "a staging file of a running process stays");
        assert!(root.join("notes.txt").exists());
        assert_eq!(std::fs::read(&kept).unwrap(), b"CURRENT ENTRY");
        // A second sweep finds nothing, and the entry still hits.
        assert_eq!(cache.sweep(), Swept { generations: 0, files: 0, bytes: 0 });
        cache.get_or_convert(&conv, b"current entry").unwrap();
        assert_eq!(conv.runs.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn generations_have_plain_names_and_plain_caches_sweep_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(Cache::open_generation(tmp.path(), "").is_err());
        assert!(Cache::open_generation(tmp.path(), "../up").is_err());
        let beside = tmp.path().join("gen-other");
        std::fs::create_dir_all(&beside).unwrap();
        let plain = Cache::open(tmp.path().join("plain")).unwrap();
        assert_eq!(plain.sweep(), Swept::default());
        assert!(beside.exists());
        assert_eq!(staging_pid(".tmp-12-3"), Some(12));
        assert_eq!(staging_pid(".tmp-12"), None);
        assert_eq!(staging_pid("0123abcd"), None);
        assert!(process_runs(std::process::id()));
    }

    #[test]
    fn version_bump_invalidates() {
        let v1 = Upper {
            version: 1,
            runs: AtomicUsize::new(0),
        };
        let v2 = Upper {
            version: 2,
            runs: AtomicUsize::new(0),
        };
        assert_ne!(CacheKey::new(&v1, b"x"), CacheKey::new(&v2, b"x"));
    }

    #[test]
    fn refuses_cache_inside_repo() {
        let inside = Path::new(env!("CARGO_MANIFEST_DIR")).join("cache");
        assert!(Cache::open(inside).is_err());
    }
}
