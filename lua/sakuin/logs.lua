-- Auto-refreshing scratch buffer over sakuin's log file. Lets you watch git
-- change detection, watcher events, and indexing in real time.

local M = {}

local log_bufnr = nil ---@type number|nil
local refresh_timer = nil ---@type vim.uv.Timer|nil

local REFRESH_MS = 1000

local level_hl = {
	ERROR = "DiagnosticError",
	["WARN "] = "DiagnosticWarn",
	["INFO "] = "DiagnosticInfo",
	DEBUG = "DiagnosticHint",
	TRACE = "Comment",
}

---@return string
local function log_file_path()
	local config = require("sakuin.config").get()
	return config.log_file or (vim.fn.stdpath("state") .. "/sakuin.log")
end

---@return string[]
local function read_log_file()
	local path = log_file_path()
	local file = io.open(path, "r")
	if not file then return { "  (no log file found at " .. path .. ")" } end

	local lines = {}
	for line in file:lines() do
		table.insert(lines, line)
	end
	file:close()

	if #lines == 0 then lines = { "  (log file is empty — try :SakuinLogs debug to enable debug logging)" } end

	return lines
end

---@param lines string[]
local function render(lines)
	if not log_bufnr or not vim.api.nvim_buf_is_valid(log_bufnr) then return end

	vim.api.nvim_set_option_value("modifiable", true, { buf = log_bufnr })
	vim.api.nvim_buf_set_lines(log_bufnr, 0, -1, false, lines)
	vim.api.nvim_set_option_value("modifiable", false, { buf = log_bufnr })

	vim.api.nvim_buf_clear_namespace(log_bufnr, -1, 0, -1)
	for i, line in ipairs(lines) do
		for level_str, hl_group in pairs(level_hl) do
			-- Level appears after the timestamp: "2026-04-25 11:58:27.123 INFO  [..."
			local col_start = line:find(level_str, 1, true)
			if col_start then
				vim.api.nvim_buf_set_extmark(log_bufnr, -1, i - 1, col_start - 1, {
					end_col = col_start - 1 + #level_str,
					hl_group = hl_group,
				})
				break
			end
		end
	end
end

local function refresh()
	if not log_bufnr or not vim.api.nvim_buf_is_valid(log_bufnr) then
		M.close()
		return
	end

	local should_scroll = {}
	for _, win in ipairs(vim.api.nvim_list_wins()) do
		if vim.api.nvim_win_get_buf(win) == log_bufnr then
			local old_line_count = vim.api.nvim_buf_line_count(log_bufnr)
			local cursor = vim.api.nvim_win_get_cursor(win)
			should_scroll[win] = (cursor[1] >= old_line_count)
		end
	end

	local lines = read_log_file()
	render(lines)

	for win, scroll in pairs(should_scroll) do
		if scroll and vim.api.nvim_win_is_valid(win) then
			local new_line_count = vim.api.nvim_buf_line_count(log_bufnr)
			vim.api.nvim_win_set_cursor(win, { new_line_count, 0 })
		end
	end
end

local function start_timer()
	if refresh_timer then return end
	local timer = vim.uv.new_timer()
	if not timer then return end
	refresh_timer = timer
	refresh_timer:start(0, REFRESH_MS, vim.schedule_wrap(refresh))
end

local function stop_timer()
	if refresh_timer then
		refresh_timer:stop()
		refresh_timer:close()
		refresh_timer = nil
	end
end

---@param opts? { level?: string }
function M.open(opts)
	opts = opts or {}

	if opts.level then
		local ok, ffi_mod = pcall(require, "sakuin.ffi")
		if ok and ffi_mod.is_loaded() then
			local success = ffi_mod.set_log_level(opts.level)
			if success then
				vim.notify("[sakuin] Log level set to " .. opts.level, vim.log.levels.INFO)
			else
				vim.notify("[sakuin] Unknown log level: " .. opts.level, vim.log.levels.ERROR)
				return
			end
		end
	end

	if log_bufnr and vim.api.nvim_buf_is_valid(log_bufnr) then
		for _, win in ipairs(vim.api.nvim_list_wins()) do
			if vim.api.nvim_win_get_buf(win) == log_bufnr then
				vim.api.nvim_set_current_win(win)
				refresh()
				return
			end
		end
		vim.cmd("botright split")
		vim.api.nvim_win_set_buf(0, log_bufnr)
		start_timer()
		refresh()
		return
	end

	log_bufnr = vim.api.nvim_create_buf(false, true)
	vim.api.nvim_set_option_value("buftype", "nofile", { buf = log_bufnr })
	vim.api.nvim_set_option_value("bufhidden", "wipe", { buf = log_bufnr })
	vim.api.nvim_set_option_value("swapfile", false, { buf = log_bufnr })
	vim.api.nvim_set_option_value("filetype", "sakuin-logs", { buf = log_bufnr })
	vim.api.nvim_buf_set_name(log_bufnr, "sakuin://logs")

	vim.cmd("botright split")
	vim.api.nvim_win_set_buf(0, log_bufnr)
	vim.api.nvim_win_set_height(0, 15)

	local buf_opts = { buffer = log_bufnr, silent = true }
	vim.keymap.set("n", "q", M.close, buf_opts)
	vim.keymap.set("n", "R", refresh, buf_opts)
	vim.keymap.set("n", "C", function()
		local ok, ffi_mod = pcall(require, "sakuin.ffi")
		if ok and ffi_mod.is_loaded() then
			ffi_mod.clear_logs()
			refresh()
		end
	end, buf_opts)

	vim.api.nvim_create_autocmd("BufWipeout", {
		buffer = log_bufnr,
		callback = function()
			stop_timer()
			log_bufnr = nil
		end,
	})

	start_timer()
	refresh()
end

function M.close()
	stop_timer()
	if log_bufnr and vim.api.nvim_buf_is_valid(log_bufnr) then
		for _, win in ipairs(vim.api.nvim_list_wins()) do
			if vim.api.nvim_win_get_buf(win) == log_bufnr then vim.api.nvim_win_close(win, true) end
		end
	end
	log_bufnr = nil
end

return M
