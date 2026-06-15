# LibAnything

Low-level filesystem indexer — walks `/` recursively and writes a binary `.anythingindex` file.

## Architecture

Recursive directory tree walk via `jwalk` from `/`, recording every file and directory.
Symlinks are skipped. Files/directories matching `IgnoreConfig` rules are excluded.
Output format is a compact binary file with header, tree data, optional drive info, and an
optionally zstd-compressed name block (see `index_format.rs`).

## Public API

### Core types

```rust
pub struct FileRecord {
    pub id: u64,
    pub parent_id: u64,
    pub name: String,
}

pub enum IndexerStatus {
    Idle,
    Running,
    Completed,
    Failed,
}
```

### IgnoreConfig — skip rules for walk & search

```rust
pub struct IgnoreConfig {
    pub skip_dir_prefixes: Vec<String>,  // dirs whose path starts with any prefix
    pub skip_file_names: Vec<String>,    // exact filenames to treat as noise
    pub skip_file_exts: Vec<String>,     // extensions to treat as noise
}

impl IgnoreConfig {
    pub fn new() -> Self;
    pub fn is_skip_dir(&self, path: &Path) -> bool;
    pub fn is_noise(&self, path: &str) -> bool;
}
```

### Indexer — walks filesystem, writes index

```rust
pub struct Indexer {
    pub compressed: bool,  // default: true — zstd-compress name block
}

impl Indexer {
    pub fn new(output: PathBuf) -> Self;
    pub fn set_ignore_config(&mut self, config: IgnoreConfig);
    pub fn start(&mut self);
    pub fn status(&self) -> IndexerStatus;
    pub fn progress(&self) -> u64;
    pub fn partial_records(&self) -> Vec<FileRecord>;
}
```

### IndexBuilder — write a `.anythingindex` from records

```rust
pub struct IndexBuilder {
    pub compressed: bool,
}

impl IndexBuilder {
    pub fn new() -> Self;
    pub fn add_entry(&mut self, id: u64, parent_id: u64, path: &str);
    pub fn add_drive(&mut self, drive: DriveInfo);
    pub fn auto_detect_drives(&mut self);
    pub fn write<P: AsRef<Path>>(&self, path: P) -> Result<(), String>;
}
```

### IndexReader — read a `.anythingindex`

```rust
pub struct IndexReader {
    pub entries: Vec<Entry>,
    pub drives: Vec<DriveInfo>,
    pub name_data: Vec<u8>,
    pub timestamp: i64,
    pub entry_count: u64,
    pub changed_drives: Vec<String>,
}

pub struct Entry {
    pub id: u32,
    pub parent_id: u32,
    pub name_off: u32,
}

pub struct DriveInfo {
    pub path: String,
    pub volume_id: Vec<u8>,
}

impl IndexReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String>;
    pub fn get_name(&self, entry: &Entry) -> &str;
    pub fn get_by_id(&self, id: u32) -> Option<&Entry>;
}
```

### Utilities

```rust
/// Detect current mount points (Linux: /proc/self/mountinfo, macOS: getfsstat).
pub fn check_drive_changes(stored: &[DriveInfo]) -> Vec<String>;

/// High-level convenience: builds index from records in one call.
pub fn build_index_file(
    records: &[FileRecord],
    path: &Path,
    compressed: bool,
    cancel: &AtomicBool,
) -> Result<(), String>;
```

## Usage

```rust
use libanything::{Indexer, IndexerStatus, IgnoreConfig};

let mut idx = Indexer::new("/home/user/.config/anything-index.anythingindex");
idx.compressed = true;

let mut cfg = IgnoreConfig::new();
cfg.skip_dir_prefixes.push("/tmp".into());
idx.set_ignore_config(cfg);

idx.start();
loop {
    match idx.status() {
        IndexerStatus::Running => eprintln!("Progress: {}", idx.progress()),
        IndexerStatus::Completed => break,
        IndexerStatus::Failed => { eprintln!("Failed"); break; }
        _ => {}
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
}
```

## Tests

```sh
cargo test
```

6 unit tests covering: lifecycle, walk, cancel, skip-dir filtering, noise filtering,
binary roundtrip (uncompressed and zstd-compressed).

## Dependencies

- `jwalk` — parallel filesystem walker
- `zstd` — name block compression
- `log` — logging facade

## Binary format

See `src/index_format.rs` for the full `.anythingindex` spec:

```
[Header 80B] [Tree Data: Entry×N] [Drive Data] [Name Block (optional zstd)]
```
