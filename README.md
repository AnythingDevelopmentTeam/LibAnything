# LibAnything

Low-level filesystem indexer — walks `/` recursively and writes a YAML index.

Index output: `~/.config/anything-index.yaml`

## Architecture

Recursive directory tree walk via `std::fs::read_dir` from `/`, recording every
file and directory. Symlinks are skipped to prevent cycles.

Linked into `searchengine` — both compile into a single `.so`/`.dll`.

## API

```rust
use libanything::{Indexer, IndexerStatus};

let mut indexer = Indexer::new("/home/user/.config/anything-index.yaml");
indexer.start();

loop {
    match indexer.status() {
        IndexerStatus::Running => println!("Progress: {}", indexer.progress()),
        IndexerStatus::Completed => break,
        IndexerStatus::Failed => { eprintln!("Failed"); break; }
        _ => {}
    }
    std::thread::sleep(Duration::from_millis(500));
}
```

## Tests

```sh
cargo test
```

4 unit tests covering: walk, cancel, YAML roundtrip, lifecycle.

## Dependencies

- `log` — logging facade
- `serde` + `serde_yaml` — YAML index serialization
- `libc` (Unix only) — POSIX FFI
