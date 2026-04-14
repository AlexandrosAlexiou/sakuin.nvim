--- sakuin.nvim — Indexed full-text search for Neovim
--- Main entry point and public API.

local M = {}

---@type boolean
local initialized = false

--- Register an indexing callback that only cares about done/error.
--- Notifies start via vim.notify, and finish/fail when the event arrives.
---@param ffi_mod table The FFI module
---@param label string Label for notifications (e.g. "Syncing")
---@param on_done? fun() Optional callback when indexing finishes successfully
local function watch_indexing(ffi_mod, label, on_done)
	local progress = require("sakuin.progress")
	local finished = false

	progress.start(label)

	ffi_mod.set_indexing_callback(function(event)
		if finished then
			return
		end
		if event.status == "progress" then
			return -- ignore intermediate progress
		end

		finished = true
		ffi_mod.set_indexing_callback(nil)

		if event.status == "done" then
			local stats = ffi_mod.stats()
			local msg = stats and string.format("%d files indexed", stats.num_docs) or "complete"
			progress.done(msg)
			if on_done then
				on_done()
			end
		else
			local err = event.error or ffi_mod.last_error() or "unknown error"
			progress.fail(label, err)
		end
	end)
end

--- Initialize the engine: load library, open index, mark as initialized.
--- Reused by both deferred_startup and async_index (first build).
---@param config table The merged configuration
---@return table|nil ffi_mod The FFI module on success, nil on failure
local function init_engine(config)
	local ffi_mod = require("sakuin.ffi")

	if ffi_mod.is_loaded() then
		return ffi_mod
	end

	local ok, err = pcall(ffi_mod.load)
	if not ok then
		vim.notify("[sakuin] Failed to load native library: " .. tostring(err), vim.log.levels.ERROR)
		return nil
	end

	local root = vim.fn.getcwd()
	local index_dir = root .. "/.sakuin"
	local rust_config = vim.json.encode({
		max_file_size = config.max_file_size,
		ignore_patterns = config.ignore_patterns or {},
		include_extensions = config.include_extensions,
		respect_gitignore = config.respect_gitignore ~= false,
	})
	local rc = ffi_mod.init(root, index_dir, rust_config)
	if rc ~= 0 then
		vim.notify("[sakuin] Failed to initialize: " .. (ffi_mod.last_error() or "unknown error"), vim.log.levels.ERROR)
		return nil
	end

	initialized = true
	return ffi_mod
end

--- Deferred startup sequence. Runs entirely off the main loop so setup()
--- returns immediately and Neovim is never blocked.
---
--- Only auto-syncs if the .sakuin/ index directory already exists.
--- If it doesn't, the user must run :SakuinBuild first.
---@param config table The merged configuration
local function deferred_startup(config)
	local root = vim.fn.getcwd()
	local index_dir = root .. "/.sakuin"

	-- No existing index — wait for the user to run :SakuinBuild
	if vim.fn.isdirectory(index_dir) == 0 then
		return
	end

	local ffi_mod = init_engine(config)
	if not ffi_mod then
		return
	end

	-- Start watcher after sync completes (or immediately if no update)
	local function start_watcher_if_enabled()
		if config.watch then
			vim.schedule(function()
				ffi_mod.start_watcher()
			end)
		end
	end

	if config.update_on_start then
		local update_rc = ffi_mod.update_index_async()
		if update_rc == 0 then
			watch_indexing(ffi_mod, "Syncing", start_watcher_if_enabled)
		else
			vim.notify(
				"[sakuin] Failed to start async update: " .. (ffi_mod.last_error() or "unknown"),
				vim.log.levels.ERROR
			)
			start_watcher_if_enabled()
		end
	else
		start_watcher_if_enabled()
	end
end

--- Setup the plugin with user configuration.
--- Returns immediately — all heavy work (library loading, index opening,
--- incremental update) runs asynchronously via vim.schedule.
---@param opts? table User configuration (see sakuin.config for defaults)
function M.setup(opts)
	local config = require("sakuin.config").apply(opts or {})

	vim.api.nvim_create_autocmd("VimLeavePre", {
		callback = function()
			local ffi_mod = require("sakuin.ffi")
			if ffi_mod.is_loaded() then
				ffi_mod.shutdown()
			end
		end,
		desc = "Shut down sakuin engine on exit",
	})

	if type(config.keymaps) == "table" then
		if config.keymaps.search then
			vim.keymap.set("n", config.keymaps.search, "<cmd>Sakuin<cr>", { desc = "Sakuin search" })
		end
		if config.keymaps.search_cword then
			vim.keymap.set(
				"n",
				config.keymaps.search_cword,
				"<cmd>SakuinCword<cr>",
				{ desc = "Sakuin search word under cursor" }
			)
			vim.keymap.set(
				"x",
				config.keymaps.search_cword,
				'"vy:<C-u>lua require("sakuin.picker").sakuin({ search = vim.fn.getreg("v") })<CR>',
				{ desc = "Sakuin search visual selection" }
			)
		end
		if config.keymaps.rebuild then
			vim.keymap.set("n", config.keymaps.rebuild, "<cmd>SakuinBuild<cr>", { desc = "Sakuin rebuild index" })
		end
	end

	vim.schedule(function()
		deferred_startup(config)
	end)
end

---@param query string
---@return table|nil
---@return string|nil
function M.search(query)
	if not initialized then
		return nil, "sakuin is not initialized yet"
	end
	return require("sakuin.ffi").search(query)
end

--- On the first :SakuinBuild, lazy-initializes the engine if needed.
---@param mode string "build" or "update"
function M.async_index(mode)
	local ffi_mod = require("sakuin.ffi")

	-- Lazy-init: if the engine isn't loaded yet (no .sakuin/ existed at startup),
	-- initialize it now so :SakuinBuild works.
	if not ffi_mod.is_loaded() then
		local config = require("sakuin.config").get()
		ffi_mod = init_engine(config) --[[@as table]]
		if not ffi_mod then
			return
		end
	end

	local progress = require("sakuin.progress")
	if progress.is_indexing then
		vim.notify("[sakuin] Indexing is already in progress", vim.log.levels.WARN)
		return
	end

	local label = mode == "build" and "Building" or "Updating"
	local fn_async = mode == "build" and ffi_mod.build_index_async or ffi_mod.update_index_async

	local config = require("sakuin.config").get()
	local function start_watcher_if_enabled()
		if config.watch then
			vim.schedule(function()
				ffi_mod.start_watcher()
			end)
		end
	end

	local rc = fn_async()
	if rc == 0 then
		watch_indexing(ffi_mod, label, start_watcher_if_enabled)
	else
		vim.notify("[sakuin] Failed to start: " .. (ffi_mod.last_error() or "unknown"), vim.log.levels.ERROR)
	end
end

return M
