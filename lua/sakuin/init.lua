local M = {}

local function start_watcher_if_enabled(ffi_mod, config)
	if config.watch then vim.schedule(function() ffi_mod.start_watcher() end) end
end

---@param ffi_mod table
---@param label string
---@param mode "build"|"update"
---@param on_done? fun()
local function watch_indexing(ffi_mod, label, mode, on_done)
	local progress = require("sakuin.progress")
	local finished = false

	progress.start(label)

	ffi_mod.set_indexing_callback(function(event)
		if finished then return end
		if event.status == "progress" then return end

		finished = true
		ffi_mod.set_indexing_callback(nil)

		if event.status == "done" then
			local msg
			if mode == "update" then
				local n = event.done or 0
				msg = n == 0 and "Index up to date" or string.format("%d file%s synced", n, n == 1 and "" or "s")
			else
				local stats = ffi_mod.stats()
				msg = stats and string.format("%d files indexed", stats.num_docs) or "complete"
			end
			progress.done(msg)
			if on_done then on_done() end
		else
			progress.fail(label, event.error or "unknown error")
		end
	end)
end

---@param config table
---@param on_ready fun(ffi_mod: table|nil)
local function init_engine_async(config, on_ready)
	local ffi_mod = require("sakuin.ffi")

	if ffi_mod.is_loaded() then
		on_ready(ffi_mod)
		return
	end

	require("sakuin.install").ensure_binary({}, function(ok)
		if not ok then
			on_ready(nil)
			return
		end

		local load_ok, load_err = pcall(ffi_mod.load)
		if not load_ok then
			vim.notify("[sakuin] Failed to load native library: " .. tostring(load_err), vim.log.levels.ERROR)
			on_ready(nil)
			return
		end

		local root = vim.fn.getcwd()
		local index_dir = root .. "/.sakuin"
		local rust_config = vim.json.encode({
			max_file_size = config.max_file_size,
			ignore_patterns = config.ignore_patterns or {},
			include_extensions = config.include_extensions,
			respect_gitignore = config.respect_gitignore ~= false,
			log_level = config.log_level or "info",
			log_file = config.log_file,
		})
		local init_ok, init_err = ffi_mod.init(root, index_dir, rust_config)
		if not init_ok then
			vim.notify("[sakuin] Failed to initialize: " .. (init_err or "unknown error"), vim.log.levels.ERROR)
			on_ready(nil)
			return
		end

		on_ready(ffi_mod)
	end)
end

--- Shut down the current engine and reinitialize for a new working directory.
--- If the new cwd has an existing index, runs an incremental update + starts watcher.
---@param config table
local function reinit_engine(config)
	local ffi_mod = require("sakuin.ffi")
	if not ffi_mod.is_loaded() then return end

	local root = vim.fn.getcwd()
	local index_dir = root .. "/.sakuin"
	local rust_config = vim.json.encode({
		max_file_size = config.max_file_size,
		ignore_patterns = config.ignore_patterns or {},
		include_extensions = config.include_extensions,
		respect_gitignore = config.respect_gitignore ~= false,
		log_level = config.log_level or "info",
		log_file = config.log_file,
	})

	local ok, reinit_err = ffi_mod.reinit(root, index_dir, rust_config)
	if not ok then
		vim.notify("[sakuin] Failed to reinitialize: " .. (reinit_err or "unknown error"), vim.log.levels.ERROR)
		return
	end

	if vim.fn.isdirectory(index_dir) == 1 then
		if config.update_on_start then
			local update_ok = ffi_mod.update_index_async()
			if update_ok then
				watch_indexing(ffi_mod, "Syncing", "update", function() start_watcher_if_enabled(ffi_mod, config) end)
			else
				start_watcher_if_enabled(ffi_mod, config)
			end
		else
			start_watcher_if_enabled(ffi_mod, config)
		end
	end
end

-- Skips if no .sakuin/ exists (user must :SakuinBuild first).
---@param config table
local function deferred_startup(config)
	local index_dir = vim.fn.getcwd() .. "/.sakuin"
	if vim.fn.isdirectory(index_dir) == 0 then return end

	init_engine_async(config, function(ffi_mod)
		if not ffi_mod then return end

		if config.update_on_start then
			local update_ok, update_err = ffi_mod.update_index_async()
			if update_ok then
				watch_indexing(ffi_mod, "Syncing", "update", function() start_watcher_if_enabled(ffi_mod, config) end)
			else
				vim.notify("[sakuin] Failed to start async update: " .. (update_err or "unknown"), vim.log.levels.ERROR)
				start_watcher_if_enabled(ffi_mod, config)
			end
		else
			start_watcher_if_enabled(ffi_mod, config)
		end
	end)
end

---@class sakuin.Config.Search
---@field limit? integer Max total results (0 = unlimited)

---@class sakuin.Config.Progress
---@field enabled? boolean

---@class sakuin.Config.Keymaps
---@field search? string|false Open search picker
---@field search_cword? string|false Search word under cursor / visual selection
---@field rebuild? string|false Full index rebuild

---@class sakuin.Config
---@field update_on_start? boolean Run incremental index update on startup
---@field watch? boolean Watch filesystem for live index updates
---@field max_file_size? integer Max file size to index, in bytes
---@field ignore_patterns? string[] Patterns to ignore (on top of .gitignore)
---@field include_extensions? string[] File extensions to index (nil = all text files detected by content)
---@field respect_gitignore? boolean Respect .gitignore / .ignore files
---@field log_level? "error"|"warn"|"info"|"debug"|"trace"|"off"
---@field log_file? string Path to the log file
---@field search? sakuin.Config.Search
---@field progress? sakuin.Config.Progress
---@field keymaps? sakuin.Config.Keymaps|false Set to false to disable all keymaps

---@param opts? sakuin.Config
function M.setup(opts)
	local config = require("sakuin.config").apply(opts or {})

	vim.api.nvim_create_autocmd("VimLeavePre", {
		callback = function()
			local ffi_mod = require("sakuin.ffi")
			if ffi_mod.is_loaded() then ffi_mod.shutdown() end
		end,
		desc = "Shut down sakuin engine on exit",
	})

	vim.api.nvim_create_autocmd("DirChanged", {
		callback = function()
			local ffi_mod = require("sakuin.ffi")
			if ffi_mod.is_loaded() then
				reinit_engine(config)
			else
				deferred_startup(config)
			end
		end,
		desc = "Reinitialize sakuin index for new working directory",
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

	vim.schedule(function() deferred_startup(config) end)
end

-- Lazy-inits engine if not already loaded.
---@param mode "build"|"update"
function M.async_index(mode)
	local config = require("sakuin.config").get()
	init_engine_async(config, function(ffi_mod)
		if not ffi_mod then return end

		local progress = require("sakuin.progress")
		if progress.is_indexing then
			vim.notify("[sakuin] Indexing is already in progress", vim.log.levels.WARN)
			return
		end

		local label = mode == "build" and "Building" or "Updating"
		local fn_async = mode == "build" and ffi_mod.build_index_async or ffi_mod.update_index_async

		local ok, err = fn_async()
		if ok then
			watch_indexing(ffi_mod, label, mode, function() start_watcher_if_enabled(ffi_mod, config) end)
		else
			vim.notify("[sakuin] Failed to start: " .. (err or "unknown"), vim.log.levels.ERROR)
		end
	end)
end

--- Delete the .sakuin/ index directory.
function M.clean()
	local ffi_mod = require("sakuin.ffi")

	-- Shut down the engine first if running (releases file handles on the index)
	if ffi_mod.is_loaded() then ffi_mod.shutdown() end

	local index_dir = vim.fn.getcwd() .. "/.sakuin"
	if vim.fn.isdirectory(index_dir) == 1 then
		vim.fn.delete(index_dir, "rf")
		vim.notify("[sakuin] Cleaned index at " .. index_dir, vim.log.levels.INFO)
	else
		vim.notify("[sakuin] No index found at " .. index_dir, vim.log.levels.WARN)
	end
end

return M
