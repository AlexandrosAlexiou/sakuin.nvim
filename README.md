Sakuin &mdash; fast full-text search for Neovim
==============================================

[<img alt="github" src="https://img.shields.io/badge/github-AlexandrosAlexiou/sakuin.nvim-8da0cb?style=for-the-badge&labelColor=555555&logo=github" height="20">](https://github.com/AlexandrosAlexiou/sakuin.nvim)
[<img alt="release" src="https://img.shields.io/github/v/release/AlexandrosAlexiou/sakuin.nvim?style=for-the-badge&color=fc8d62&logo=rust" height="20">](https://github.com/AlexandrosAlexiou/sakuin.nvim/releases)
[<img alt="neovim" src="https://img.shields.io/badge/neovim-0.10+-66c2a5?style=for-the-badge&labelColor=555555&logo=neovim" height="20">](https://neovim.io)
[<img alt="build status" src="https://img.shields.io/github/actions/workflow/status/AlexandrosAlexiou/sakuin.nvim/ci.yml?branch=main&style=for-the-badge" height="20">](https://github.com/AlexandrosAlexiou/sakuin.nvim/actions?query=branch%3Amain)

Sakuin (索引) is a fast full-text search plugin for Neovim, powered by
[Tantivy](https://github.com/quickwit-oss/tantivy).

Working on massive codebases, the usual live-grep tools start to drag once a
project gets big enough. Sakuin builds a persistent index up front so queries
come back in milliseconds, even across hundreds of thousands of files, and
streams results into a [snacks.nvim picker](https://github.com/folke/snacks.nvim/blob/main/docs/picker.md)
as you type.

```lua
{
  "AlexandrosAlexiou/sakuin.nvim",
  build = function()
    require("sakuin.install").build()
  end,
  dependencies = { "folke/snacks.nvim" },
  config = function()
    require("sakuin").setup {}
  end,
}
```

*Requirements: Neovim 0.10+ on LuaJIT, [snacks.nvim](https://github.com/folke/snacks.nvim), and a prebuilt binary or Rust toolchain*<br>
*Windows is not currently supported, see [Installation](#installation)*

<br>

## Getting started

1. Open a project and run `:SakuinBuild` to create the index for the first
   time. The index is written under Neovim's cache directory
   (`:echo stdpath("cache") .. "/sakuin"`), never inside the project tree, so
   nothing needs to be gitignored and git-aware tools watching the repo are
   left alone.
2. Open the search picker with `<leader>si` and start typing. Results stream in
   as you type.
3. From that point on, the index is maintained automatically. On startup,
   changed files are synced. A filesystem watcher keeps it current while you
   work.

<br>

## Overview

The idea is to pay the cost of reading the codebase once. Sakuin walks the
project, writes every text file into a Tantivy index on disk, and answers
queries from that index rather than scanning files on every keystroke. Search
speed therefore does not depend on project size.

- **Instant queries at any scale** &mdash; the index is built once and queried
  from disk.

- **Streaming results** &mdash; results appear as you type, delivered in
  batches from a background thread.

- **Incremental indexing** &mdash; only changed files are re-indexed on
  startup, keeping sync fast even on large projects.

- **Live index updates** &mdash; a filesystem watcher re-indexes files as you
  save, with no manual intervention.

- **Respects `.gitignore`** &mdash; ignored files, build artifacts, lockfiles,
  and `node_modules` are skipped automatically.

- **Git-aware change detection** &mdash; after a checkout, rebase, or stash,
  libgit2 (in-process, no `git` subprocesses) computes exactly which files
  changed so only those are re-indexed.

- **File-based logging** &mdash; structured logs are written to disk with a
  built-in log viewer (`:SakuinLogs`) for real-time debugging.

<br>

## Installation

> [!WARNING]
> **Windows is not currently supported.** Neovim does not export
> `uv_async_send` on Windows, which this plugin requires for thread-safe async
> notifications. See [neovim/neovim#40307](https://github.com/neovim/neovim/discussions/40307#discussioncomment-17436804)
> for details.

With [lazy.nvim](https://github.com/folke/lazy.nvim), use the snippet at the
top of this page. The `build` step downloads a prebuilt binary, or compiles
from source if you have a Rust toolchain.

<br>

## Configuration

All values are optional:

```lua
require("sakuin").setup({
  -- Run incremental index update on startup
  update_on_start = true,

  -- Watch filesystem for live index updates
  watch = true,

  -- Max file size to index (bytes)
  max_file_size = 1024 * 1024, -- 1 MB

  -- Patterns to ignore (on top of .gitignore)
  ignore_patterns = {
    "node_modules", ".git", "target", "dist", "build",
    "*.min.js", "*.map", "package-lock.json", "yarn.lock",
  },

  -- Glob whitelist (gitignore syntax); only matching files are indexed.
  -- nil = no whitelist. e.g. { "*", "src/**" }
  include_patterns = nil,

  -- File extensions to index (nil = all text files detected by content)
  include_extensions = nil,

  -- Respect .gitignore / .ignore files
  respect_gitignore = true,

  -- Log level: "error", "warn", "info", "debug", "trace", "off"
  log_level = "info",

  -- Path to the log file
  log_file = vim.fn.stdpath("state") .. "/sakuin.log",

  -- Search options
  search = {
    limit = 10000,     -- max total results (0 = unlimited)
    debounce = 150,    -- idle ms after the last keystroke before searching
  },

  -- Progress notifications
  progress = {
    enabled = true,
  },

  -- Keymaps (set to false to disable all)
  keymaps = {
    search = "<leader>si",       -- open search picker
    search_cword = "<leader>sW", -- search word under cursor / visual selection
    rebuild = nil,               -- full index rebuild (no default binding)
  },
})
```

<br>

## Per-project config

Drop a `sakuin.toml` at the root of a repo to override the indexing settings
for that project only. It is meant to be checked into the repo, so everyone
working on the codebase indexes it the same way, independent of their personal
`setup()` config.

```toml
# sakuin.toml — at the project root
respect_gitignore  = true
max_file_size      = 1048576
ignore_patterns    = ["build", "vendor", "*.gen.go"]   # [SearchableFiles.ignore]
include_patterns   = ["*"]                              # [SearchableFiles.include]
include_extensions = ["go", "lua", "rs"]
```

This mirrors a VsChromium `vs-chromium-project.txt`: `ignore_patterns` is
`[SearchableFiles.ignore]` and `include_patterns` is the glob whitelist
`[SearchableFiles.include]` (gitignore syntax &mdash; `*`, `src/**`,
`Makefile` all work). With `include_patterns` set, only files matching at least
one pattern are indexed; ignores still apply on top. `include_extensions` is a
simpler extension-only shorthand and can be used instead of or alongside it.

Only these five fields are recognized &mdash; they are exactly the settings
that describe *what a project indexes*. Anything else (keymaps, logging, the
filesystem watcher, search debounce) stays in `setup()` since it's a personal
editor preference, not a property of the repo.

- A field set in `sakuin.toml` **wins** over the same field in `setup()`.
  Fields it doesn't set fall back to `setup()`, then the built-in defaults.

- Arrays **replace**, they don't merge: `ignore_patterns` in the file fully
  determines the list.

- A missing or malformed `sakuin.toml` is a warning (see `:SakuinLogs`), not an
  error; indexing falls back to the `setup()` config.

<br>

## Commands

<table>
<tr><th>command</th><th>description</th></tr>
<tr><td><code>:Sakuin [query]</code></td><td>Open the search picker (optionally with initial query)</td></tr>
<tr><td><code>:SakuinCword</code></td><td>Search word under cursor or visual selection</td></tr>
<tr><td><code>:SakuinBuild</code></td><td>Full index rebuild</td></tr>
<tr><td><code>:SakuinClean</code></td><td>Stop engine and wipe index</td></tr>
<tr><td><code>:SakuinSync</code></td><td>Incremental index sync</td></tr>
<tr><td><code>:SakuinStats</code></td><td>Show index statistics</td></tr>
<tr><td><code>:SakuinLogs [level]</code></td><td>Open log viewer (optionally set log level, e.g. <code>:SakuinLogs debug</code>)</td></tr>
</table>

<br>

## Keymaps

<table>
<tr><th>key</th><th>description</th></tr>
<tr><td><code>&lt;leader&gt;si</code></td><td>Open search picker</td></tr>
<tr><td><code>&lt;leader&gt;sW</code></td><td>Search word under cursor or visual selection</td></tr>
</table>

<br>

## Health check

```vim
:checkhealth sakuin
```

<br>

#### License

<sup>
Licensed under the <a href="LICENSE">MIT license</a>.
</sup>
