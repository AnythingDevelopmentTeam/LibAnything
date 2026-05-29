# LibAnything

Low-level filesystem indexer — the core scanning engine for Anything.

## Architecture

- **Windows:** reads raw volume (`\\.\C:`) via `CreateFileW` (direct MFT access)
- **POSIX:** recursive directory tree walk via `std::fs::read_dir`

Exports a C ABI (`cdylib`) for dynamic loading from any language.

## Build

```sh
cargo build --release
```

Output: `target/release/libanything.{dll,so,dylib}`

## FFI

| Function | Description |
|----------|-------------|
| `init_indexer(volume_path)` | Open volume handle |
| `shutdown_indexer()` | Close volume handle |
| `run_indexation(callback)` | Walk filesystem, call callback per entry |
| `scan_directories(roots)` | Public Rust API — returns `Vec<String>` of file paths |

## Dependencies

- `log` — logging facade
- `libc` (Unix only) — POSix FFI
