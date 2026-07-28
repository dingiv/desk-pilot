# FileLoader

Dev/prod asset path resolver for cargo crates. Finds files under a crate's
own `assets/` directory regardless of working directory, `target/` depth, or
whether the binary is running from a build tree or an installed location.

## Quick start

### 1. Declare namespaces in `Cargo.toml`

```toml
[package.metadata.shared]
DICT  = { dev = "assets/dict",   prod = "/usr/share/my-app/dict" }
MODEL = { dev = "../../assets",  prod = "~/.my-app/models" }
```

- **dev**: relative to `CARGO_MANIFEST_DIR` (the crate root).
- **prod**: absolute path or `~/`-prefixed path (resolved via `$HOME`).

### 2. Add a `build.rs`

```rust
// build.rs
fn main() {
    shared::emit_namespaces();
}
```

This reads `[package.metadata.shared]` from your `Cargo.toml` and generates
a compile-time table consumed by `loader!()`.

### 3. Use in code

```rust
let loader = shared::loader!("assets");
//                 ^^^^^^^^  subdirectory under the crate root (default: "assets")

// Namespaced paths — dev/prod auto-selected:
let path = loader.resolve("DICT::rime-ice.tsv").unwrap();
let data = loader.read_str("DICT::rime-ice.tsv")?;

// Bare (non-namespaced) paths — always relative to assets/:
let logo = loader.read("icon.png")?;   // → assets/icon.png
```

## How paths are resolved

`loader.resolve(path)` tries candidates in order, returning the first that
exists on disk:

| Mode   | Prefix      | Candidate path                              |
|--------|-------------|---------------------------------------------|
| dev    | bare        | `{CARGO_MANIFEST_DIR}/assets/{path}`        |
| dev    | bare        | `{exe_dir}/assets/{path}`                   |
| dev    | bare        | `./assets/{path}`                           |
| dev    | `NS::rel`   | `{CARGO_MANIFEST_DIR}/{NS.dev}/rel`         |
| prod   | bare        | `{exe_dir}/assets/{path}`                   |
| prod   | bare        | `./assets/{path}`                           |
| prod   | `NS::rel`   | `{NS.prod}/rel` (with `~/` expansion)       |

Mode detection: `is_dev()` returns `true` when `CARGO_MANIFEST_DIR` is set
(i.e. running under `cargo`). In a released binary, `is_dev()` returns `false`
and prod paths are used.

## Real-world example: swift-ime

```toml
# apps/swift-ime/Cargo.toml
[package.metadata.shared]
DICT = { dev = "assets/dict", prod = "/usr/share/swift-ime/dict" }
```

```rust
// apps/swift-ime/src/main.rs
let loader = shared::loader!("assets");
let engine = ImeEngine::new();

// Dev: reads from <crate>/assets/dict/rime-ice.tsv
// Prod (after cmake install): reads from /usr/share/swift-ime/dict/rime-ice.tsv
if let Some(p) = loader.resolve("DICT::rime-ice.tsv") {
    engine.load_dict(&p.to_string_lossy())?;
}
```

```cmake
# CMakeLists.txt — install dict files for production
install(FILES assets/dict/rime-ice.tsv
        DESTINATION "${CMAKE_INSTALL_DATADIR}/swift-ime/dict")
```

## Real-world example: aura-asr (model files)

```toml
# crates/aura-asr/Cargo.toml
[package.metadata.shared]
MODELS = { dev = "../../assets/models", prod = "~/.desk-pilot/models" }
```

```rust
// crates/aura-asr/src/executor.rs
let loader = shared::loader!("assets");
let model_path = loader.resolve("MODELS::sensevoice.onnx")
    .expect("model not found");
```

- Dev: model files live in repo-root `assets/models/` (shared across crates).
- Prod: user downloads models to `~/.desk-pilot/models/`.

## API reference

| Method | Returns | Description |
|--------|---------|-------------|
| `loader!()` | `FileLoader` | Default assets subdir |
| `loader!("assets")` | `FileLoader` | Custom subdir |
| `resolve("path")` | `Option<PathBuf>` | First existing candidate |
| `resolve("NS::rel")` | `Option<PathBuf>` | Namespace-aware resolve |
| `read("path")` | `Result<Vec<u8>>` | Read binary file |
| `read_str("path")` | `Result<String>` | Read UTF-8 text file |
| `write("path", data)` | `Result<()>` | Write binary file |
| `write_str("path", s)` | `Result<()>` | Write text file |
| `exists("path")` | `bool` | Check if any candidate exists |
| `candidates("rel")` | `Vec<PathBuf>` | All candidate paths (don't check existence) |

## Why not just `Path::new("assets/foo")`?

A hard-coded relative path `assets/foo` breaks when:

1. **Cargo workspace**: `cargo run -p my-app` runs from the workspace root,
   not the crate directory.
2. **`cargo test`**: test binaries execute from `target/debug/deps/`, three
   levels deep.
3. **Installed binary**: after `cmake --install`, `assets/` doesn't exist
   relative to `/usr/bin/my-app`.

FileLoader solves all three by checking multiple candidate locations.
