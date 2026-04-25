--- sakuin.nvim — User commands and autocommands.
--- Auto-loaded by Neovim from plugin/ directory.

if vim.g.loaded_sakuin then
  return
end
vim.g.loaded_sakuin = true

---@param bytes number
---@return string
local function format_bytes(bytes)
  if bytes < 1024 then
    return bytes .. " B"
  elseif bytes < 1024 * 1024 then
    return string.format("%.1f KB", bytes / 1024)
  elseif bytes < 1024 * 1024 * 1024 then
    return string.format("%.1f MB", bytes / (1024 * 1024))
  else
    return string.format("%.1f GB", bytes / (1024 * 1024 * 1024))
  end
end

---@return string
local function get_visual_selection()
  -- Exit visual mode to update '< and '> marks
  vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes("<Esc>", true, false, true), "nx", false)
  local start_pos = vim.api.nvim_buf_get_mark(0, "<")
  local end_pos = vim.api.nvim_buf_get_mark(0, ">")
  local lines = vim.api.nvim_buf_get_text(0, start_pos[1] - 1, start_pos[2], end_pos[1] - 1, end_pos[2] + 1, {})
  return table.concat(lines, " ")
end

vim.api.nvim_create_user_command("Sakuin", function(opts)
  local has_snacks, _ = pcall(require, "snacks")
  if not has_snacks then
    vim.notify("[sakuin] snacks.nvim is required for the search UI", vim.log.levels.ERROR)
    return
  end
  local progress = require("sakuin.progress")
  if progress.is_indexing then
    vim.notify("[sakuin] Indexing is in progress, please wait…", vim.log.levels.WARN)
    return
  end
  local ffi_mod = require("sakuin.ffi")
  if not ffi_mod.is_loaded() then
    vim.notify("[sakuin] No index found. Run :SakuinBuild first.", vim.log.levels.WARN)
    return
  end
  local search = (opts.args and opts.args ~= "") and opts.args or nil
  require("sakuin.picker").sakuin({ search = search })
end, {
  nargs = "?",
  desc = "Open sakuin indexed search",
})

vim.api.nvim_create_user_command("SakuinCword", function(opts)
  local has_snacks, _ = pcall(require, "snacks")
  if not has_snacks then
    vim.notify("[sakuin] snacks.nvim is required for the search UI", vim.log.levels.ERROR)
    return
  end
  local progress = require("sakuin.progress")
  if progress.is_indexing then
    vim.notify("[sakuin] Indexing is in progress, please wait…", vim.log.levels.WARN)
    return
  end
  local ffi_mod = require("sakuin.ffi")
  if not ffi_mod.is_loaded() then
    vim.notify("[sakuin] No index found. Run :SakuinBuild first.", vim.log.levels.WARN)
    return
  end

  local text
  if opts.range > 0 then
    -- Called from visual mode (via :'<,'>SakuinCword)
    text = get_visual_selection()
  else
    text = vim.fn.expand("<cword>")
  end

  if text and text ~= "" then
    require("sakuin.picker").sakuin({ search = text })
  end
end, {
  range = true,
  desc = "Search word under cursor or visual selection with sakuin",
})

vim.api.nvim_create_user_command("SakuinBuild", function()
  local ok, sakuin = pcall(require, "sakuin")
  if not ok then
    vim.notify("[sakuin] Not initialized. Call require('sakuin').setup() first.", vim.log.levels.ERROR)
    return
  end
  sakuin.async_index("build")
end, {
  desc = "Full index rebuild (async with progress)",
})

vim.api.nvim_create_user_command("SakuinUpdate", function()
  local ok, sakuin = pcall(require, "sakuin")
  if not ok then
    vim.notify("[sakuin] Not initialized. Call require('sakuin').setup() first.", vim.log.levels.ERROR)
    return
  end
  sakuin.async_index("update")
end, {
  desc = "Incremental index update (async with progress)",
})

vim.api.nvim_create_user_command("SakuinStats", function()
  local ok, ffi_mod = pcall(require, "sakuin.ffi")
  if not ok or not ffi_mod.is_loaded() then
    vim.notify("[sakuin] Not initialized. Call require('sakuin').setup() first.", vim.log.levels.ERROR)
    return
  end
  local stats, err = ffi_mod.stats()
  if stats then
    local msg = string.format(
      "[sakuin] %d files indexed | %d segments | %s on disk\n  root: %s",
      stats.num_docs,
      stats.num_segments,
      format_bytes(stats.index_size_bytes),
      stats.project_root
    )
    vim.notify(msg, vim.log.levels.INFO)
  else
    vim.notify("[sakuin] " .. (err or "unknown error"), vim.log.levels.ERROR)
  end
end, {
  desc = "Show index statistics",
})

vim.api.nvim_create_user_command("SakuinLogs", function(opts)
  local level = (opts.args and opts.args ~= "") and opts.args or nil
  require("sakuin.logs").open({ level = level })
end, {
  nargs = "?",
  complete = function()
    return { "error", "warn", "info", "debug", "trace", "off" }
  end,
  desc = "Open sakuin log viewer (optional: set log level, e.g. :SakuinLogs debug)",
})
