local M = {}

local initialized = false

local function start_watcher_if_enabled(ffi_mod, config)
	if config.watch then
		vim.schedule(function()
			ffi_mod.start_watcher()
		end)
	end
end

-- Drives one indexing run: notifies start, ignores intermediate progress events,
-- and routes the terminal event to progress.done / progress.fail.
---@param ffi_mod table
---@param label string
---@param on_done? fun()
local function watch_indexing(ffi_mod, label, on_done)
	local progress = require("sakuin.progress")
	local finished = false

	progress.start(label)

	ffi_mod.set_indexing_callback(function(event)
		if finished then
			return
		end
		if event.status == "progress" then
			return
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

---@param config table
---@return table|nil ffi_mod
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
		log_level = config.log_level or "info",
		log_file = config.log_file,
	})
	local rc = ffi_mod.init(root, index_dir, rust_config)
	if rc ~= 0 then
		vim.notify("[sakuin] Failed to initialize: " .. (ffi_mod.last_error() or "unknown error"), vim.log.levels.ERROR)
		return nil
	end

	initialized = true
	return ffi_mod
end

-- Runs off the main loop so setup() returns immediately. Skips entirely when
-- there's no .sakuin/ yet — the user must run :SakuinBuild first, otherwise
-- we'd build an index unprompted on every cwd that has the plugin loaded.
---@param config table
local function deferred_startup(config)
	local index_dir = vim.fn.getcwd() .. "/.sakuin"
	if vim.fn.isdirectory(index_dir) == 0 then
		return
	end

	local ffi_mod = init_engine(config)
	if not ffi_mod then
		return
	end

	if config.update_on_start then
		local update_rc = ffi_mod.update_index_async()
		if update_rc == 0 then
			watch_indexing(ffi_mod, "Syncing", function()
				start_watcher_if_enabled(ffi_mod, config)
			end)
		else
			vim.notify(
				"[sakuin] Failed to start async update: " .. (ffi_mod.last_error() or "unknown"),
				vim.log.levels.ERROR
			)
			start_watcher_if_enabled(ffi_mod, config)
		end
	else
		start_watcher_if_enabled(ffi_mod, config)
	end
end

---@param opts? table
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

-- Lazy-inits the engine on first :SakuinBuild — startup skips init when there's
-- no .sakuin/ yet, so the first build has to bring the engine up itself.
---@param mode "build"|"update"
function M.async_index(mode)
	local ffi_mod = require("sakuin.ffi")

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

	local rc = fn_async()
	if rc == 0 then
		watch_indexing(ffi_mod, label, function()
			start_watcher_if_enabled(ffi_mod, config)
		end)
	else
		vim.notify("[sakuin] Failed to start: " .. (ffi_mod.last_error() or "unknown"), vim.log.levels.ERROR)
	end
end

return M
