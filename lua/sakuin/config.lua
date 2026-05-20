local M = {}

M.defaults = {
	update_on_start = true,
	watch = true,
	max_file_size = 1024 * 1024, -- 1 MB
	ignore_patterns = {
		"node_modules",
		"target",
		"dist",
		"build",
		"*.min.js",
		"*.map",
		"package-lock.json",
		"yarn.lock",
	},

	-- Glob whitelist (gitignore syntax). If set, only files matching at least
	-- one pattern are indexed (e.g. { "*", "src/**" }). nil = no whitelist.
	include_patterns = nil,

	-- If set, only index files with these extensions. nil = all text files.
	include_extensions = nil,

	-- When false, gitignored files are also indexed.
	respect_gitignore = true,

	-- Log level for the file logger: "error", "warn", "info", "debug", "trace", "off".
	-- Use :SakuinLogs to view logs, or :SakuinLogs debug to change level at runtime.
	log_level = "info",

	-- Path to the log file. Logs are appended here and viewable via :SakuinLogs.
	log_file = vim.fn.stdpath("state") .. "/sakuin.log",

	search = {
		-- 0 = unlimited. Stops search early once enough results exist.
		limit = 10000,
		-- Idle ms after the last keystroke before a search is submitted.
		debounce = 150,
	},

	progress = {
		enabled = true,
	},

	-- Set to false to disable all keymaps.
	keymaps = {
		search = "<leader>si",
		search_cword = "<leader>sW",
		rebuild = nil,
	},
}

M.current = vim.deepcopy(M.defaults)

---@param opts table
---@return table
function M.apply(opts)
	M.current = vim.tbl_deep_extend("force", vim.deepcopy(M.defaults), opts or {})
	return M.current
end

---@return table
function M.get() return M.current end

return M
