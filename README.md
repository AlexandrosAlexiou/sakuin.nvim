# sakuin.nvim (索引)

Full-text search for Neovim built for large codebases. Powered by [Tantivy](https://github.com/quickwit-oss/tantivy).

A fast, indexed alternative to live grep for large codebases. Builds a persistent full-text index so queries return in milliseconds across 100,000+ files, streamed into a [snacks.nvim picker](https://github.com/folke/snacks.nvim/blob/main/docs/picker.md) as you type.

## Requirements

- **Neovim** >= 0.10
- **LuaJIT** (standard Lua not supported)
- [snacks.nvim](https://github.com/folke/snacks.nvim) for the picker UI
- A prebuilt binary or a **Rust toolchain** to compile from source

## Installation

### [lazy.nvim](https://github.com/folke/lazy.nvim)

```lua
{
  "AlexandrosAlexiou/sakuin.nvim",
  build = function() require("sakuin.install").ensure_binary() end,
  dependencies = { "folke/snacks.nvim" },
  opts = {},
}
```

## Getting Started

1. Open a project and run `:SakuinBuild` to create the index for the first time. The index is written to `.sakuin/` at your project root — add it to your `.gitignore`.
2. Open the search picker with `<leader>si` and start typing. Results stream in as you type.
3. From that point on, the index is maintained automatically. On startup, changed files are synced. A filesystem watcher keeps it current while you work.

## Features

- **Instant queries at any scale**: the index is built once and queried from disk, so search speed does not depend on project size
- **Streaming results**: results appear as you type, delivered in batches from a background thread
- **Incremental indexing**: only changed files are re-indexed on startup, keeping sync fast even on large projects
- **Live index updates**: a filesystem watcher re-indexes files as you save, with no manual intervention
- **Respects `.gitignore`**: ignored files, build artifacts, lockfiles, and `node_modules` are skipped automatically

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

  -- File extensions to index (nil = all text files detected by content)
  include_extensions = nil,

  -- Respect .gitignore / .ignore files
  respect_gitignore = true,

  -- Search options
  search = {
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

## Keymaps

| Key | Description |
| --- | --- |
| `<leader>si` | Open search picker |
| `<leader>sW` | Search word under cursor or visual selection |

## Health Check

```vim
:checkhealth sakuin
```

## License

[MIT](LICENSE)
