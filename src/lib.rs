use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

mod index_format;
pub use index_format::*;

// ──────────────────────────────────────────────────────────────────────────────
// Data
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FileRecord {
    pub id: u64,
    pub parent_id: u64,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexerStatus {
    Idle,
    Running,
    Completed,
    Failed,
}

// ──────────────────────────────────────────────────────────────────────────────
// IgnoreConfig — rules for skipping dirs/files during walk & search
// ──────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct IgnoreConfig {
    pub skip_dir_prefixes: Vec<String>,
    pub skip_file_names: Vec<String>,
    pub skip_file_exts: Vec<String>,
}

impl IgnoreConfig {
    pub fn new() -> Self {
        IgnoreConfig {
            skip_dir_prefixes: Vec::new(),
            skip_file_names: Vec::new(),
            skip_file_exts: Vec::new(),
        }
    }

    /// Directory path prefixes that should not be entered during the walk.
    pub fn is_skip_dir(&self, path: &Path) -> bool {
        let s = path.to_string_lossy();
        self.skip_dir_prefixes
            .iter()
            .any(|p| s == *p || (s.starts_with(p) && s.as_bytes().get(p.len()) == Some(&b'/')))
    }

    /// File that should be filtered out from search results by name.
    pub fn is_noise(&self, path: &str) -> bool {
        let name = match Path::new(path).file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return false,
        };
        let lower_name = name.to_lowercase();

        if self.skip_file_names.iter().any(|n| lower_name == *n) {
            return true;
        }

        if lower_name.ends_with('~') {
            return true;
        }

        if let Some(ext) = Path::new(name).extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            if self.skip_file_exts.iter().any(|e| ext_lower == *e) {
                return true;
            }
        }

        false
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Indexer — walks from /, writes .anythingindex
// ──────────────────────────────────────────────────────────────────────────────

pub struct Indexer {
    cancel: Arc<AtomicBool>,
    status: Arc<RwLock<IndexerStatus>>,
    progress: Arc<AtomicU64>,
    records: Arc<RwLock<Vec<FileRecord>>>,
    output: PathBuf,
    pub compressed: bool,
    ignore: IgnoreConfig,
}

impl Indexer {
    pub fn new(output: PathBuf) -> Self {
        Indexer {
            cancel: Arc::new(AtomicBool::new(false)),
            status: Arc::new(RwLock::new(IndexerStatus::Idle)),
            progress: Arc::new(AtomicU64::new(0)),
            records: Arc::new(RwLock::new(Vec::new())),
            output,
            compressed: true,
            ignore: IgnoreConfig::new(),
        }
    }

    pub fn set_ignore_config(&mut self, config: IgnoreConfig) {
        self.ignore = config;
    }

    pub fn start(&mut self) {
        if *self.status.read().unwrap() == IndexerStatus::Running {
            log::warn!("Indexer already running");
            return;
        }

        *self.status.write().unwrap() = IndexerStatus::Running;
        self.cancel.store(false, Ordering::SeqCst);
        self.progress.store(0, Ordering::SeqCst);
        *self.records.write().unwrap() = Vec::new();

        let cancel = self.cancel.clone();
        let status = self.status.clone();
        let progress = self.progress.clone();
        let shared_records = self.records.clone();
        let output = self.output.clone();
        let compressed = self.compressed;
        let ignore = Arc::new(self.ignore.clone());

        std::thread::spawn(move || {
            let start = std::time::Instant::now();
            let mut records = Vec::new();

            records.push(FileRecord {
                id: 1,
                parent_id: 0,
                name: "/".into(),
            });
            progress.fetch_add(1, Ordering::SeqCst);

            *shared_records.write().unwrap() = records.clone();

            let cancel_watchdog = cancel.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(45));
                cancel_watchdog.store(true, Ordering::SeqCst);
            });

            walk(Path::new("/"), &mut records, &shared_records, &cancel, &progress, &ignore);

            if cancel.load(Ordering::SeqCst) {
                *status.write().unwrap() = IndexerStatus::Idle;
                return;
            }

            let result = build_index_file(&records, &output, compressed, &cancel);

            match result {
                Ok(()) => {
                    let elapsed = start.elapsed();
                    log::info!(
                        "Index written to {:?} ({} entries, {:.1}s)",
                        output,
                        progress.load(Ordering::SeqCst),
                        elapsed.as_secs_f64()
                    );
                    *status.write().unwrap() = IndexerStatus::Completed;
                }
                Err(e) => {
                    if e == "cancelled" {
                        *status.write().unwrap() = IndexerStatus::Idle;
                    } else {
                        log::error!("Failed to write index: {}", e);
                        *status.write().unwrap() = IndexerStatus::Failed;
                    }
                }
            }
        });
    }

    pub fn status(&self) -> IndexerStatus {
        *self.status.read().unwrap()
    }

    pub fn progress(&self) -> u64 {
        self.progress.load(Ordering::SeqCst)
    }

    pub fn partial_records(&self) -> Vec<FileRecord> {
        self.records.read().unwrap().clone()
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Filesystem walk
// ──────────────────────────────────────────────────────────────────────────────

fn walk(
    root: &Path,
    records: &mut Vec<FileRecord>,
    shared: &Arc<RwLock<Vec<FileRecord>>>,
    cancel: &AtomicBool,
    progress: &AtomicU64,
    ignore: &Arc<IgnoreConfig>,
) {
    let ignore_clone = ignore.clone();
    let walker = jwalk::WalkDir::new(root)
        .process_read_dir(move |_depth, _path, _state, children| {
            children.retain(|e| {
                let Ok(e) = e else { return false };
                if e.file_type().is_symlink() { return false; }
                let path = e.path();
                !ignore_clone.is_skip_dir(&path)
            });
        });

    let mut parent_ids: HashMap<PathBuf, u64> = HashMap::new();
    let mut id_counter: u64 = 2;
    let mut last_sync = 0usize;

    parent_ids.insert(root.to_path_buf(), 1);

    for entry in walker {
        if cancel.load(Ordering::SeqCst) {
            return;
        }

        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        if cancel.load(Ordering::SeqCst) {
            return;
        }

        let my_id = id_counter;
        id_counter += 1;

        if let Some(name) = path.to_str() {
            let parent_path = path.parent().unwrap_or(root);
            let parent_id = parent_ids.get(parent_path).copied().unwrap_or(1);
            parent_ids.insert(path.to_path_buf(), my_id);

            records.push(FileRecord {
                id: my_id,
                parent_id,
                name: name.to_string(),
            });
            progress.fetch_add(1, Ordering::SeqCst);
        }

        if records.len() - last_sync >= 5000 {
            let new_slice = &records[last_sync..];
            if let Ok(mut guard) = shared.write() {
                guard.extend_from_slice(new_slice);
            }
            last_sync = records.len();
        }
    }

    if last_sync < records.len() {
        let new_slice = &records[last_sync..];
        if let Ok(mut guard) = shared.write() {
            guard.extend_from_slice(new_slice);
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ignore() -> IgnoreConfig {
        IgnoreConfig {
            skip_dir_prefixes: vec![
                "/proc".into(),
                "/sys".into(),
                "/dev".into(),
                "/run".into(),
                "/snap".into(),
                "/lost+found".into(),
                "/tmp".into(),
                "/boot".into(),
                "/lib".into(),
                "/lib64".into(),
                "/usr/lib".into(),
                "/usr/lib64".into(),
                "/usr/share/zoneinfo".into(),
                "/usr/share/doc".into(),
                "/usr/share/help".into(),
                "/usr/share/man".into(),
                "/usr/include".into(),
                "/usr/src".into(),
                "/var/cache".into(),
                "/var/log".into(),
                "/var/tmp".into(),
                "/opt".into(),
                "/sysroot".into(),
                "/var/lib/docker".into(),
                "/var/lib/flatpak".into(),
            ],
            skip_file_names: vec![
                "thumbs.db".into(),
                "desktop.ini".into(),
                ".ds_store".into(),
                "icon\r".into(),
            ],
            skip_file_exts: vec![
                "tmp".into(),
                "temp".into(),
                "bak".into(),
            ],
        }
    }

    #[test]
    fn test_indexer_lifecycle() {
        let tmp = std::env::temp_dir().join("libanything-test.anythingindex");
        let indexer = Indexer::new(tmp.clone());
        assert_eq!(indexer.status(), IndexerStatus::Idle);
        assert_eq!(indexer.progress(), 0);
        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_walk_small_dir() {
        let tmp = std::env::temp_dir().join("libanything-walk-test");
        let _ = std::fs::create_dir_all(&tmp);
        std::fs::write(tmp.join("a.txt"), "hello").ok();
        std::fs::write(tmp.join("b.txt"), "world").ok();
        let sub = tmp.join("sub");
        let _ = std::fs::create_dir_all(&sub);
        std::fs::write(sub.join("c.txt"), "deep").ok();

        let mut records = Vec::new();
        records.push(FileRecord {
            id: 1,
            parent_id: 0,
            name: "/".into(),
        });
        let cancel = AtomicBool::new(false);
        let progress = AtomicU64::new(0);
        let shared = Arc::new(RwLock::new(Vec::new()));
        let ignore = Arc::new(IgnoreConfig::new());

        walk(&tmp, &mut records, &shared, &cancel, &progress, &ignore);

        assert!(!records.is_empty());
        assert!(progress.load(Ordering::SeqCst) > 0);
        assert!(!shared.read().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_cancel() {
        let cancel = AtomicBool::new(true);
        let mut records = Vec::new();
        let progress = AtomicU64::new(0);
        let shared = Arc::new(RwLock::new(Vec::new()));
        let ignore = Arc::new(IgnoreConfig::new());
        records.push(FileRecord {
            id: 1,
            parent_id: 0,
            name: "/".into(),
        });

        walk(Path::new("/"), &mut records, &shared, &cancel, &progress, &ignore);
        assert_eq!(records.len(), 1);
    }

    #[test]
    fn test_skip_dir_filtering() {
        let ig = test_ignore();
        assert!(ig.is_skip_dir(Path::new("/proc")));
        assert!(ig.is_skip_dir(Path::new("/proc/self")));
        assert!(ig.is_skip_dir(Path::new("/sys/class")));
        assert!(ig.is_skip_dir(Path::new("/dev")));
        assert!(ig.is_skip_dir(Path::new("/dev/shm")));
        assert!(ig.is_skip_dir(Path::new("/run/user/1000")));
        assert!(ig.is_skip_dir(Path::new("/snap/core/1234")));
        assert!(ig.is_skip_dir(Path::new("/lost+found")));
        assert!(ig.is_skip_dir(Path::new("/var/lib/docker/overlay2")));
        assert!(ig.is_skip_dir(Path::new("/var/lib/flatpak/repo")));
        assert!(ig.is_skip_dir(Path::new("/tmp")));
        assert!(ig.is_skip_dir(Path::new("/tmp/foo")));
        assert!(ig.is_skip_dir(Path::new("/boot")));
        assert!(ig.is_skip_dir(Path::new("/lib/x86_64-linux-gnu")));
        assert!(ig.is_skip_dir(Path::new("/lib64")));
        assert!(ig.is_skip_dir(Path::new("/usr/lib/python3")));
        assert!(ig.is_skip_dir(Path::new("/usr/share/zoneinfo/America")));
        assert!(ig.is_skip_dir(Path::new("/usr/share/doc/bash")));
        assert!(ig.is_skip_dir(Path::new("/usr/include/linux")));
        assert!(ig.is_skip_dir(Path::new("/var/cache/apt")));
        assert!(ig.is_skip_dir(Path::new("/var/log/syslog")));
        assert!(ig.is_skip_dir(Path::new("/opt/google")));
        assert!(ig.is_skip_dir(Path::new("/sysroot")));
        assert!(!ig.is_skip_dir(Path::new("/home/user/proc")));
        assert!(!ig.is_skip_dir(Path::new("/home/user/dev")));
    }

    #[test]
    fn test_noise_filter() {
        let ig = test_ignore();
        assert!(ig.is_noise("/home/user/thumbs.db"));
        assert!(ig.is_noise("/home/user/desktop.ini"));
        assert!(ig.is_noise("/home/user/.ds_store"));
        assert!(!ig.is_noise("/home/user/report.pdf"));
        // extension filters
        assert!(ig.is_noise("/tmp/foo.tmp"));
        assert!(ig.is_noise("/tmp/foo.temp"));
        assert!(ig.is_noise("/tmp/foo.bak"));
        assert!(!ig.is_noise("/home/user/notes.txt"));
        // trailing tilde
        assert!(ig.is_noise("/home/user/backup~"));
    }

    #[test]
    fn test_index_file_roundtrip() {
        let tmp = std::env::temp_dir().join("libanything-roundtrip.anythingindex");
        let records = vec![
            FileRecord { id: 1, parent_id: 0, name: "/".into() },
            FileRecord { id: 2, parent_id: 1, name: "/home".into() },
            FileRecord { id: 3, parent_id: 2, name: "/home/user".into() },
        ];
        let cancel = AtomicBool::new(false);

        build_index_file(&records, &tmp, false, &cancel).unwrap();

        let reader = IndexReader::open(&tmp).unwrap();
        assert_eq!(reader.entry_count, 3);
        assert_eq!(reader.get_name(&reader.entries[0]), "/");
        assert_eq!(reader.get_name(&reader.entries[2]), "/home/user");

        let _ = std::fs::remove_file(&tmp);
    }

    #[test]
    fn test_compressed_roundtrip() {
        let tmp = std::env::temp_dir().join("libanything-compressed.anythingindex");
        let records = vec![
            FileRecord { id: 1, parent_id: 0, name: "/".into() },
            FileRecord { id: 2, parent_id: 1, name: "/home/user/document.txt".into() },
        ];
        let cancel = AtomicBool::new(false);

        build_index_file(&records, &tmp, true, &cancel).unwrap();
        let reader = IndexReader::open(&tmp).unwrap();
        assert_eq!(reader.entry_count, 2);
        assert_eq!(reader.get_name(&reader.entries[1]), "/home/user/document.txt");

        let _ = std::fs::remove_file(&tmp);
    }
}
