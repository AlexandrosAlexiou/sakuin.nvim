if vim.g.loaded_sakuin then return end
vim.g.loaded_sakuin = true

local function get_visual_selection()
	-- Exit visual mode to update '< and '> marks
	vim.api.nvim_feedkeys(vim.api.nvim_replace_termcodes("<Esc>", true, false, true), "nx", false)
	local start_pos = vim.api.nvim_buf_get_mark(0, "<")
	local end_pos = vim.api.nvim_buf_get_mark(0, ">")
	local lines = vim.api.nvim_buf_get_text(0, start_pos[1] - 1, start_pos[2], end_pos[1] - 1, end_pos[2] + 1, {})
	return table.concat(lines, " ")
end

local function search_preflight()
	if not pcall(require, "snacks") then
		vim.notify("[sakuin] snacks.nvim is required for the search UI", vim.log.levels.ERROR)
		return false
	end
	if require("sakuin.progress").is_indexing then
		vim.notify("[sakuin] Indexing is in progress, please wait…", vim.log.levels.WARN)
		return false
	end
	if require("sakuin.install").is_installing() then
		vim.notify("[sakuin] Native library is being built, please wait…", vim.log.levels.WARN)
		return false
	end
	if not require("sakuin.ffi").is_loaded() then
		vim.notify("[sakuin] No index found. Run :SakuinBuild first.", vim.log.levels.WARN)
		return false
	end
	return true
end

vim.api.nvim_create_user_command("Sakuin", function(opts)
	if not search_preflight() then return end
	local search = opts.args ~= "" and opts.args or nil
	require("sakuin.picker").sakuin({ search = search })
end, {
	nargs = "?",
	desc = "Open sakuin indexed search",
})

vim.api.nvim_create_user_command("SakuinCword", function(opts)
	if not search_preflight() then return end
	local text = opts.range > 0 and get_visual_selection() or vim.fn.expand("<cword>")
	if text and text ~= "" then require("sakuin.picker").sakuin({ search = text }) end
end, {
	range = true,
	desc = "Search word under cursor or visual selection with sakuin",
})

vim.api.nvim_create_user_command("SakuinBuild", function() require("sakuin").async_index("build") end, {
	desc = "Full index rebuild (async with progress)",
})

vim.api.nvim_create_user_command("SakuinClean", function() require("sakuin").clean() end, {
	desc = "Delete the index for the current working directory",
})

vim.api.nvim_create_user_command("SakuinSync", function() require("sakuin").async_index("update") end, {
	desc = "Incremental index sync (async with progress)",
})

vim.api.nvim_create_user_command("SakuinStats", function()
	local ffi_mod = require("sakuin.ffi")
	if not ffi_mod.is_loaded() then
		vim.notify("[sakuin] Not initialized. Call require('sakuin').setup() first.", vim.log.levels.ERROR)
		return
	end
	local stats, err = ffi_mod.stats()
	if stats then
		local format_bytes = require("sakuin.util").format_bytes
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
	complete = function() return { "error", "warn", "info", "debug", "trace", "off" } end,
	desc = "Open sakuin log viewer (optional: set log level, e.g. :SakuinLogs debug)",
})
