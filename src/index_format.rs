// ═══════════════════════════════════════════════════════════════════════════════
// .anythingindex binary format
// ═══════════════════════════════════════════════════════════════════════════════

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
//
// Layout:
//   [Header] [Tree Data] [Drive Data] [Name Block]
//
// Header (80 bytes):
//   magic            [u8; 8]     "ANYIDX\1"
//   version          u32 LE      1
//   flags            u16 LE      bit 0: names_compressed (zstd)
//                                bit 1: has_drive_info
//   _reserved1       u16 LE
//   timestamp        i64 LE      unix ms
//   entry_count      u64 LE
//   name_block_size  u64 LE      uncompressed size of name block
//   name_block_csize u64 LE      compressed size, 0 if uncompressed
//   drive_count      u32 LE
//   tree_entry_count u32 LE      == entry_count
//   drive_data_off   u64 LE      byte offset from file start
//   name_block_off   u64 LE      byte offset from file start
//   _reserved2       [u8; 8]
//
// Tree Data (immediately after header, 80..80+entry_count*12):
//   [Entry; entry_count]
//   Entry { id: u32, parent_id: u32, name_off: u32 }
//
// Drive Data (at drive_data_off):
//   [DriveEntry; drive_count]
//   DriveEntry { path_len: u32, path: [u8; path_len], vol_id_len: u8, vol_id: [u8; vol_id_len] }
//
// Name Block (at name_block_off):
//   null-terminated UTF-8 paths. If names_compressed flag is set,
//   the entire block is zstd-compressed.
// ═══════════════════════════════════════════════════════════════════════════════

use std::io::Write;
use std::path::Path;
use std::sync::atomic::AtomicBool;

use crate::FileRecord;

// ── Constants ────────────────────────────────────────────────────────────────

const MAGIC: [u8; 8] = [0x41, 0x4E, 0x59, 0x49, 0x44, 0x58, 0x01, 0x00]; // "ANYIDX\1\0"
const HEADER_SIZE: u64 = 80;
const ENTRY_SIZE: u64 = 12;
const FORMAT_VERSION: u32 = 1;

// ── Flags ────────────────────────────────────────────────────────────────────

const FLAG_NAMES_COMPRESSED: u16 = 0x0001;
const FLAG_HAS_DRIVE_INFO: u16 = 0x0002;



// ── Drive info ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DriveInfo {
    pub path: String,
    pub volume_id: Vec<u8>,
}

fn detect_drives() -> Vec<DriveInfo> {
    let mut drives = Vec::new();

    #[cfg(target_os = "linux")]
    {
        // Parse /proc/self/mountinfo for real filesystems
        if let Ok(content) = std::fs::read_to_string("/proc/self/mountinfo") {
            for line in content.lines() {
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.len() < 10 {
                    continue;
                }
                // fields[3] = root, fields[4] = mount point, fields[5] = options,
                // fields[8] = super options
                let mount_point = fields[4];
                // Skip pseudo filesystems
                let skip = ["/proc", "/sys", "/dev", "/run", "/snap", "/var/lib/docker",
                            "/sys/", "/proc/", "/dev/"];
                if skip.iter().any(|p| mount_point.starts_with(p) || *p == mount_point) {
                    // But include /dev/shm which is real enough
                    if mount_point != "/dev/shm" {
                        continue;
                    }
                }
                // Get device ID
                if let Ok(meta) = std::fs::metadata(mount_point) {
                    let dev = meta.dev();
                    drives.push(DriveInfo {
                        path: mount_point.to_string(),
                        volume_id: dev.to_le_bytes().to_vec(),
                    });
                }
            }
        }
        // Always include / as a fallback
        if !drives.iter().any(|d| d.path == "/") {
            if let Ok(meta) = std::fs::metadata("/") {
                drives.push(DriveInfo {
                    path: "/".to_string(),
                    volume_id: meta.dev().to_le_bytes().to_vec(),
                });
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        // Use getfsstat via libc
        let mut buf_size = unsafe { libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
        if buf_size > 0 {
            let size = buf_size as usize * std::mem::size_of::<libc::statfs>();
            let mut buf: Vec<libc::statfs> = Vec::with_capacity(buf_size as usize);
            let count = unsafe { libc::getfsstat(buf.as_mut_ptr(), size as i32, libc::MNT_NOWAIT) };
            if count > 0 {
                unsafe { buf.set_len(count as usize); }
                for fs in &buf {
                    let mount_point = unsafe { std::ffi::CStr::from_ptr(fs.f_mntonname.as_ptr()) }
                        .to_string_lossy().into_owned();
                    let skip = ["/proc", "/sys", "/dev"];
                    if skip.iter().any(|p| mount_point.starts_with(p) || *p == mount_point) {
                        continue;
                    }
                    drives.push(DriveInfo {
                        path: mount_point,
                        volume_id: fs.f_fsid.val.to_le_bytes().to_vec(),
                    });
                }
            }
        }
    }

    drives
}

/// Check if stored drives are still present. Returns paths that are missing/changed.
pub fn check_drive_changes(stored: &[DriveInfo]) -> Vec<String> {
    let mut changed = Vec::new();
    for drive in stored {
        let path = Path::new(&drive.path);
        if !path.exists() {
            changed.push(drive.path.clone());
            continue;
        }
        if !drive.volume_id.is_empty() {
            if let Ok(meta) = path.metadata() {
                let current_dev = meta.dev();
                let stored_dev = u64::from_le_bytes(
                    drive.volume_id[..8].try_into().unwrap_or([0; 8])
                );
                if current_dev != stored_dev {
                    changed.push(drive.path.clone());
                }
            }
        }
    }
    changed
}

// ── Builder (write path) ─────────────────────────────────────────────────────

pub struct IndexBuilder {
    entries: Vec<(u32, u32, String)>, // (id, parent_id, path)
    drives: Vec<DriveInfo>,
    pub compressed: bool,
}

impl IndexBuilder {
    pub fn new() -> Self {
        IndexBuilder {
            entries: Vec::new(),
            drives: Vec::new(),
            compressed: false,
        }
    }

    pub fn add_entry(&mut self, id: u64, parent_id: u64, path: &str) {
        self.entries.push((id as u32, parent_id as u32, path.to_string()));
    }

    pub fn add_drive(&mut self, drive: DriveInfo) {
        self.drives.push(drive);
    }

    pub fn auto_detect_drives(&mut self) {
        self.drives = detect_drives();
    }

    pub fn write<P: AsRef<Path>>(&self, path: P) -> Result<(), String> {
        let output = path.as_ref();

        // Sort entries by id to ensure deterministic order
        let mut sorted = self.entries.clone();
        sorted.sort_by_key(|e| e.0);

        // Build name block
        let mut name_data = Vec::<u8>::new();
        let mut name_offsets: Vec<u32> = Vec::with_capacity(sorted.len());
        for (_, _, name) in &sorted {
            name_offsets.push(name_data.len() as u32);
            name_data.extend_from_slice(name.as_bytes());
            name_data.push(0); // null terminator
        }

        let name_block_size = name_data.len() as u64;

        // Compress name block if requested
        let compressed: bool = self.compressed;
        let name_block_csize: u64;
        let final_name_data: Vec<u8>;

        if compressed && name_block_size > 0 {
            let cdata = zstd::encode_all(&name_data[..], 3)
                .map_err(|e| format!("zstd compress failed: {}", e))?;
            name_block_csize = cdata.len() as u64;
            final_name_data = cdata;
        } else {
            name_block_csize = 0;
            final_name_data = name_data;
        }

        // Compute offsets
        let entry_count = sorted.len() as u64;
        let tree_data_size = entry_count * ENTRY_SIZE;

        // Drive data
        let mut drive_bytes = Vec::new();
        for d in &self.drives {
            let path_bytes = d.path.as_bytes();
            drive_bytes.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
            drive_bytes.extend_from_slice(path_bytes);
            drive_bytes.push(d.volume_id.len() as u8);
            drive_bytes.extend_from_slice(&d.volume_id);
        }
        let drive_data_off = HEADER_SIZE + tree_data_size;

        // Check: ensure name_block_off doesn't overlap with drive data
        // Layout: [Header] [Tree Data] [Drive Data] [Name Block]
        let name_block_off = drive_data_off + drive_bytes.len() as u64;

        // Build header
        let mut flags: u16 = 0;
        if compressed { flags |= FLAG_NAMES_COMPRESSED; }
        if !self.drives.is_empty() { flags |= FLAG_HAS_DRIVE_INFO; }

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;

        // Write file
        let file = std::fs::File::create(output)
            .map_err(|e| format!("create failed: {}", e))?;
        let mut buf = std::io::BufWriter::new(file);

        macro_rules! w {
            ($data:expr) => {
                buf.write_all($data).map_err(|e| format!("io: {}", e))?
            };
        }

        w!(&MAGIC);
        w!(&FORMAT_VERSION.to_le_bytes());
        w!(&flags.to_le_bytes());
        w!(&0u16.to_le_bytes()); // reserved1
        w!(&timestamp.to_le_bytes());
        w!(&entry_count.to_le_bytes());
        w!(&name_block_size.to_le_bytes());
        w!(&name_block_csize.to_le_bytes());
        w!(&(self.drives.len() as u32).to_le_bytes());
        w!(&(entry_count as u32).to_le_bytes());
        w!(&drive_data_off.to_le_bytes());
        w!(&name_block_off.to_le_bytes());
        w!(&[0u8; 8]); // reserved2

        // Write tree data (each entry: 12 bytes)
        for (i, &(id, parent_id, _)) in sorted.iter().enumerate() {
            w!(&id.to_le_bytes());
            w!(&parent_id.to_le_bytes());
            w!(&name_offsets[i].to_le_bytes());
        }

        // Write drive data
        w!(&drive_bytes);

        // Write name block
        w!(&final_name_data);

        buf.flush().map_err(|e| format!("flush: {}", e))?;

        log::info!(
            "Index written: {} entries, {} drives, name block {} bytes{}",
            entry_count,
            self.drives.len(),
            name_block_size,
            if compressed { format!(" (compressed {})", name_block_csize) } else { String::new() }
        );

        Ok(())
    }
}

// ── Reader ───────────────────────────────────────────────────────────────────

pub struct IndexReader {
    pub entries: Vec<Entry>,
    pub drives: Vec<DriveInfo>,
    pub name_data: Vec<u8>,
    pub timestamp: i64,
    pub entry_count: u64,
    pub changed_drives: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: u32,
    pub parent_id: u32,
    pub name_off: u32,
}

impl IndexReader {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, String> {
        let data = std::fs::read(path.as_ref())
            .map_err(|e| format!("read failed: {}", e))?;

        if data.len() < HEADER_SIZE as usize {
            return Err("file too small".into());
        }

        // Read header fields using raw byte access (packed struct)
        let magic: [u8; 8] = data[0..8].try_into().unwrap();
        if magic != MAGIC {
            return Err("bad magic".into());
        }
        let version = u32::from_le_bytes(data[8..12].try_into().unwrap());
        if version != FORMAT_VERSION {
            return Err(format!("unsupported version {}", version));
        }
        let flags = u16::from_le_bytes(data[12..14].try_into().unwrap());
        let entry_count = u64::from_le_bytes(data[24..32].try_into().unwrap());
        let _name_block_size = u64::from_le_bytes(data[32..40].try_into().unwrap());
        let drive_count = u32::from_le_bytes(data[48..52].try_into().unwrap());
        let drive_data_off = u64::from_le_bytes(data[56..64].try_into().unwrap());
        let name_block_off = u64::from_le_bytes(data[64..72].try_into().unwrap());

        let compressed = (flags & FLAG_NAMES_COMPRESSED) != 0;
        let has_drives = (flags & FLAG_HAS_DRIVE_INFO) != 0;
        let timestamp = i64::from_le_bytes(data[16..24].try_into().unwrap());

        let tree_size = (entry_count * ENTRY_SIZE) as usize;
        let tree_start = HEADER_SIZE as usize;

        // Read tree data
        let tree_end = tree_start + tree_size;
        if tree_end > data.len() {
            return Err("truncated tree data".into());
        }
        let tree_slice = &data[tree_start..tree_end];

        let entries: Vec<Entry> = tree_slice.chunks(ENTRY_SIZE as usize).map(|chunk| {
            let id = u32::from_le_bytes(chunk[0..4].try_into().unwrap());
            let parent_id = u32::from_le_bytes(chunk[4..8].try_into().unwrap());
            let name_off = u32::from_le_bytes(chunk[8..12].try_into().unwrap());
            Entry { id, parent_id, name_off }
        }).collect();

        // Read drive data
        let drives = if has_drives && (drive_data_off as usize) + 4 <= data.len() {
            let mut drv = Vec::new();
            let mut pos = drive_data_off as usize;
            for _ in 0..drive_count {
                if pos + 4 > data.len() { break; }
                let path_len = u32::from_le_bytes(data[pos..pos+4].try_into().unwrap());
                pos += 4;
                if pos + path_len as usize > data.len() { break; }
                let path = String::from_utf8_lossy(&data[pos..pos + path_len as usize]).into_owned();
                pos += path_len as usize;
                if pos >= data.len() { break; }
                let vol_len = data[pos];
                pos += 1;
                if pos + vol_len as usize > data.len() { break; }
                let vol_id = data[pos..pos + vol_len as usize].to_vec();
                pos += vol_len as usize;
                drv.push(DriveInfo { path, volume_id: vol_id });
            }
            drv
        } else {
            Vec::new()
        };

        // Read name block
        let name_data = if (name_block_off as usize) < data.len() {
            let raw = &data[name_block_off as usize..];
            if compressed {
                zstd::decode_all(raw)
                    .map_err(|e| format!("zstd decompress failed: {}", e))?
            } else {
                raw.to_vec()
            }
        } else {
            Vec::new()
        };

        // Check for changed drives
        let changed_drives = check_drive_changes(&drives);

        Ok(IndexReader {
            entries,
            drives,
            name_data,
            timestamp,
            entry_count,
            changed_drives,
        })
    }

    pub fn get_name(&self, entry: &Entry) -> &str {
        let start = entry.name_off as usize;
        let remaining = &self.name_data[start..];
        let end = remaining.iter().position(|&b| b == 0).unwrap_or(remaining.len());
        std::str::from_utf8(&remaining[..end]).unwrap_or("")
    }

    pub fn get_by_id(&self, id: u32) -> Option<&Entry> {
        // Binary search since entries are sorted by id
        self.entries.binary_search_by_key(&id, |e| e.id).ok().map(|i| &self.entries[i])
    }
}

// ── Integrate with Indexer ───────────────────────────────────────────────────

pub fn build_index_file(
    records: &[FileRecord],
    path: &Path,
    compressed: bool,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut builder = IndexBuilder::new();
    builder.compressed = compressed;
    builder.auto_detect_drives();

    for record in records {
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("cancelled".into());
        }
        builder.add_entry(record.id, record.parent_id, &record.name);
    }

    builder.write(path)
}
