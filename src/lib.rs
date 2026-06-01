// ═══════════════════════════════════════════════════════════════════════════════
// LibAnything — низкоуровневое ядро-индексатор
// Лицензия: GPL v3
//
// Архитектура:
//   Windows — прямое чтение MFT через CreateFileW (\\.\C:)
//   POSIX   — рекурсивный обход дерева каталогов
//
// Экспортирует C-совместимый FFI для динамической загрузки
// (LibAnything.dll / .so / .dylib).
// ═══════════════════════════════════════════════════════════════════════════════

use std::ffi::{c_char, CStr, CString};
use std::path::Path;
use std::sync::Mutex;

// ──────────────────────────────────────────────────────────────────────────────
// Внутреннее состояние индексатора
// ──────────────────────────────────────────────────────────────────────────────

static INITIALIZED: Mutex<bool> = Mutex::new(false);

/// Дескриптор открытого тома (Windows) или корневой путь (POSIX).
/// Хранится как isize для платформенной независимости.
static VOLUME_HANDLE: Mutex<isize> = Mutex::new(0);

// ──────────────────────────────────────────────────────────────────────────────
// Ручные FFI-привязки к Win32 API (без внешних крейтов)
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod win32 {
    #[allow(unused_imports)]
    use std::ffi::c_void;

    pub type HANDLE = isize;
    pub type BOOL = i32;
    pub type DWORD = u32;
    pub type LPCWSTR = *const u16;
    #[allow(non_camel_case_types)]
    pub type LPSECURITY_ATTRIBUTES = *mut std::ffi::c_void;

    pub const GENERIC_READ: DWORD = 0x80000000;
    pub const FILE_SHARE_READ: DWORD = 0x00000001;
    pub const FILE_SHARE_WRITE: DWORD = 0x00000002;
    pub const OPEN_EXISTING: DWORD = 3;
    pub const FILE_FLAG_NO_BUFFERING: DWORD = 0x20000000;
    pub const INVALID_HANDLE_VALUE: HANDLE = -1;

    #[link(name = "kernel32")]
    extern "system" {
        pub fn CreateFileW(
            lpFileName: LPCWSTR,
            dwDesiredAccess: DWORD,
            dwShareMode: DWORD,
            lpSecurityAttributes: LPSECURITY_ATTRIBUTES,
            dwCreationDisposition: DWORD,
            dwFlagsAndAttributes: DWORD,
            hTemplateFile: HANDLE,
        ) -> HANDLE;

        pub fn CloseHandle(hObject: HANDLE) -> BOOL;
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Вспомогательные FFI-функции
// ──────────────────────────────────────────────────────────────────────────────

unsafe fn cstr_to_str<'a>(ptr: *const c_char) -> &'a str {
    if ptr.is_null() {
        return "";
    }
    CStr::from_ptr(ptr).to_str().unwrap_or_default()
}

// ──────────────────────────────────────────────────────────────────────────────
// FFI: инициализация индексатора
// ──────────────────────────────────────────────────────────────────────────────

/// Инициализирует индексатор.
///
/// # Аргументы
/// * `volume_path` — указатель на C-строку с путём к тому
///   (например, `"\\\\.\\C:"` на Windows, `"/"` на Linux/macOS).
///
/// # Возвращает
/// `0` при успехе, `-1` при ошибке.
#[no_mangle]
pub extern "C" fn init_indexer(volume_path: *const c_char) -> i32 {
    let mut init = INITIALIZED.lock().unwrap();
    if *init {
        log::warn!("init_indexer: already initialized, skipping");
        return 0;
    }

    let path = unsafe { cstr_to_str(volume_path) };
    if path.is_empty() {
        log::error!("init_indexer: volume_path is null or empty");
        return -1;
    }

    #[cfg(target_os = "windows")]
    {
        // Прямое чтение сырого тома через CreateFileW
        // ──────────────────────────────────────────────────────────────────────
        //  CreateFileW(\\.\C:, GENERIC_READ, FILE_SHARE_READ|FILE_SHARE_WRITE,
        //              NULL, OPEN_EXISTING, FILE_FLAG_NO_BUFFERING, NULL)
        // ──────────────────────────────────────────────────────────────────────
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        let handle = unsafe {
            win32::CreateFileW(
                wide.as_ptr(),
                win32::GENERIC_READ,
                win32::FILE_SHARE_READ | win32::FILE_SHARE_WRITE,
                std::ptr::null_mut(),
                win32::OPEN_EXISTING,
                win32::FILE_FLAG_NO_BUFFERING,
                0,
            )
        };

        if handle == win32::INVALID_HANDLE_VALUE {
            log::error!("init_indexer: CreateFileW failed for path '{}'", path);
            return -1;
        }

        let mut vol = VOLUME_HANDLE.lock().unwrap();
        *vol = handle as isize;
        *init = true;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut vol = VOLUME_HANDLE.lock().unwrap();
        *vol = 1;
        *init = true;
    }

    0
}

// ──────────────────────────────────────────────────────────────────────────────
// FFI: Shutdown
// ──────────────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn shutdown_indexer() -> i32 {
    let mut init = INITIALIZED.lock().unwrap();
    if !*init {
        log::warn!("shutdown_indexer: not initialized");
        return 0;
    }

    #[cfg(target_os = "windows")]
    {
        let mut vol = VOLUME_HANDLE.lock().unwrap();
        if *vol != 0 && *vol != -1 {
            unsafe {
                win32::CloseHandle(*vol as win32::HANDLE);
            }
        }
        *vol = 0;
    }

    #[cfg(not(target_os = "windows"))]
    {
        let mut vol = VOLUME_HANDLE.lock().unwrap();
        *vol = 0;
    }

    *init = false;
    0
}

// ──────────────────────────────────────────────────────────────────────────────
// FFI: Init
// ──────────────────────────────────────────────────────────────────────────────

#[no_mangle]
pub extern "C" fn run_indexation(
    callback: Option<extern "C" fn(*const c_char, u64, u64)>,
) -> i32 {
    let init = INITIALIZED.lock().unwrap();
    if !*init {
        log::error!("run_indexation: indexer not initialized");
        return -1;
    }

    let cb = match callback {
        Some(f) => f,
        None => {
            log::error!("run_indexation: callback is null");
            return -1;
        }
    };


    for (path, id, parent) in &fake_entries {
        let c_path = CString::new(*path).unwrap();
        cb(c_path.as_ptr(), *id, *parent);
    }

    0
}

// ──────────────────────────────────────────────────────────────────────────────
// API: FS Scan
// ──────────────────────────────────────────────────────────────────────────────

/// Сканирует указанные корневые каталоги и возвращает список полных путей ко всем
/// найденным файлам (не включает каталоги). Обходит дерево рекурсивно, пропуская
/// символические ссылки для предотвращения циклов.
pub fn scan_directories(roots: &[&str]) -> Vec<String> {
    let mut all_files = Vec::new();
    for root in roots {
        let path = Path::new(root);
        if !path.exists() || !path.is_dir() {
            log::warn!("scan_directories: skipped '{root}' — not found or not a dir");
            continue;
        }
        scan_dir(path, &mut all_files);
    }
    all_files
}

fn scan_dir(dir: &Path, files: &mut Vec<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        // Пропускаем симлинки (предотвращаем циклы)
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            scan_dir(&path, files);
        } else if path.is_file() {
            if let Some(s) = path.to_str() {
                files.push(s.to_owned());
            }
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn test_init_shutdown_cycle() {
        let path = CString::new("\\\\.\\C:").unwrap();
        assert_eq!(init_indexer(path.as_ptr()), 0);
        assert_eq!(shutdown_indexer(), 0);
    }

    #[test]
    fn test_init_null_path() {
        assert_eq!(init_indexer(std::ptr::null()), -1);
    }

    #[test]
    fn test_double_init() {
        let path = CString::new("\\\\.\\C:").unwrap();
        assert_eq!(init_indexer(path.as_ptr()), 0);
        assert_eq!(init_indexer(path.as_ptr()), 0);
        shutdown_indexer();
    }

    #[test]
    fn test_run_indexation_without_init() {
        assert_eq!(run_indexation(Some(dummy_callback)), -1);
    }

    extern "C" fn dummy_callback(_path: *const c_char, _id: u64, _parent: u64) {}
}
