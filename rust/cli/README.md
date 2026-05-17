# sakuin-cli

Debug CLI for the sakuin search engine. Lets you build, sync, search, and inspect an index without opening Neovim.

## Build

From the `rust/` directory:

```bash
# debug build (fast compile, slower binary)
cargo build --bin sakuin-cli

# release build (slow compile, optimised)
cargo build --bin sakuin-cli --release
```

Binaries land at:
- `target/debug/sakuin-cli`
- `target/release/sakuin-cli`

To install system-wide:

```bash
cargo install --path . --bin sakuin-cli
```

This copies the binary to `~/.cargo/bin/sakuin-cli`, which is on your `$PATH` with a standard Rust installation.

## Usage

```
sakuin-cli <COMMAND>

Commands:
  build   Build the full index for a project directory
  sync    Incrementally sync the index (re-index changed files, remove deleted ones)
  search  Search the index and print matching lines
  stats   Print index statistics
```

The index is stored in `<root>/.sakuin` by default. Override with `--index-dir`.

### build

```bash
sakuin-cli build <root> [--index-dir <dir>]
```

Full index rebuild — clears the existing index and re-indexes all files.

```bash
sakuin-cli build ~/myproject
# Indexed 312 file(s) → ~/myproject/.sakuin
```

### sync

```bash
sakuin-cli sync <root> [--index-dir <dir>]
```

Incremental sync — re-indexes changed files and removes deleted ones.

```bash
sakuin-cli sync ~/myproject
# +3 added  ~1 updated  -0 removed
```

### search

```bash
sakuin-cli search <root> <query> [--index-dir <dir>] [--limit <n>]
```

Search the index and print matching lines. Default limit is 20 results; pass `--limit 0` for unlimited.

```bash
sakuin-cli search ~/myproject "thread_pool_size"
# src/pool.rs:4:5  thread_pool_size: usize,
# src/pool.rs:10:15  Self { thread_pool_size, max_queue_length: 1024 }
# (2 result(s))

sakuin-cli search ~/myproject "Vec3<f32>" --limit 5
sakuin-cli search ~/myproject "std::sync::Arc" --limit 0
```

### stats

```bash
sakuin-cli stats <root> [--index-dir <dir>]
```

Print index statistics.

```bash
sakuin-cli stats ~/myproject
# docs:     312
# segments: 4
# size:     1048576 bytes
# root:     /home/user/myproject
```
