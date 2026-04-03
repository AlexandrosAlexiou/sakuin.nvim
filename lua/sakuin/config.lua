--- sakuin.nvim — Configuration management.

local M = {}

--- Default configuration values.
M.defaults = {
  -- Automatically run incremental index update on setup()
  update_on_start = true,

  -- Start filesystem watcher for live index updates
  watch = true,

  -- Maximum file size to index (bytes). Files larger than this are skipped.
  max_file_size = 1024 * 1024, -- 1 MB

  -- Additional patterns to ignore (on top of .gitignore)
  ignore_patterns = {
    "node_modules",
    ".git",
    "target",
    "dist",
    "build",
    "*.min.js",
    "*.map",
    "package-lock.json",
    "yarn.lock",
  },

  -- File extensions to include. nil = all text files.
  include_extensions = nil,

  -- When true (default), .gitignore / .ignore files are respected.
  -- Set to false to index every text file in the tree (including gitignored files).
  respect_gitignore = true,

  -- Search defaults
  search = {
    -- Number of results per streaming batch delivered to the picker.
    -- Smaller = faster time-to-first-result, larger = fewer picker refreshes.
    batch_size = 500,
    -- Maximum total results to return. 0 = unlimited.
    -- Like snacks.nvim's limit_live, this stops the search once enough results
    -- exist. Results beyond this limit are never produced, keeping the UI responsive.
    limit = 10000,
  },

  -- Picker display options
  display = {
    -- Show file icons (requires nvim-web-devicons or mini.icons)
    icons = true,
    -- Show treesitter syntax highlighting on code snippets
    treesitter = true,
    -- Show column number in results
    show_col = true,
  },

  -- Progress reporting options
  progress = {
    -- Enable progress notifications during indexing
    enabled = true,
  },

  -- Keymaps (set to false to disable)
  keymaps = {
    search = "<leader>si",
    search_cword = "<leader>sW",
    rebuild = nil,
  },
}

--- The active configuration (set after setup).
M.current = vim.deepcopy(M.defaults)

--- Apply user configuration on top of defaults.
---@param opts table User overrides
---@return table config The merged configuration
function M.apply(opts)
  M.current = vim.tbl_deep_extend("force", vim.deepcopy(M.defaults), opts or {})
  return M.current
end

--- Get the current configuration.
---@return table
function M.get()
  return M.current
end

return M
