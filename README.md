# sakuin.nvim

Indexed full-text search for Neovim. Powered by [Tantivy](https://github.com/quickwit-oss/tantivy).

Builds a persistent full-text index of your project and searches it instantly
via a [snacks.nvim](https://github.com/folke/snacks.nvim) picker.

## Features

- **Tantivy-powered index**: persistent, incremental, fast to build and fast to query
- **Streaming results**: results appear as you type, delivered in batches from a background thread
- **File watcher**: index stays up to date automatically via filesystem events
- **Snacks picker**: integrates with snacks.nvim's picker for a familiar UI
- **Respects .gitignore**: skips ignored files, node_modules, build artifacts, etc.

## Requirements

- **Neovim** >= 0.10
- **LuaJIT** (standard Lua not supported)
- [snacks.nvim](https://github.com/folke/snacks.nvim) for the picker UI
- A prebuilt binary or a **Rust toolchain** to compile from source

## Installation

### [lazy.nvim](https://github.com/folke/lazy.nvim)

```lua
{
  "alexiou/sakuin.nvim",
  build = function() require("sakuin.install").ensure_binary() end,
  dependencies = { "folke/snacks.nvim" },
  opts = {},
}
```

## Configuration

Default configuration — all values are optional:

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

  -- File extensions to include (nil = all text files)
  include_extensions = nil,

  -- Respect .gitignore / .ignore files
  respect_gitignore = true,

  -- Search options
  search = {
    batch_size = 500,  -- results per streaming batch
    limit = 10000,     -- max total results (0 = unlimited)
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

## Commands

| Command | Description |
| --- | --- |
| `:Sakuin [query]` | Open the search picker (optionally with initial query) |
| `:SakuinCword` | Search word under cursor or visual selection |
| `:SakuinBuild` | Full index rebuild |
| `:SakuinUpdate` | Incremental index update |
| `:SakuinStats` | Show index statistics |

## How It Works

1. Run `:SakuinBuild` to create the index for the first time — this builds a Tantivy index in `.sakuin/` at your project root
2. On subsequent startups, `setup()` detects the existing `.sakuin/` directory and automatically syncs it (new/changed files indexed, deleted files removed)
3. A filesystem watcher keeps the index current as you edit
4. When you open the picker, your query goes to a persistent Rust search thread that streams results back via `uv_async_send`

The index is stored on disk and reused across sessions. Only changed files are re-indexed on startup.

## Health Check

```vim
:checkhealth sakuin
```

## License

[MIT](LICENSE)
