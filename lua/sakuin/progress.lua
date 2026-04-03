--- sakuin.nvim — Indexing state and simple vim.notify progress.

local M = {}

--- Whether indexing/sync is currently in progress.
---@type boolean
M.is_indexing = false

--- Check if progress notifications are enabled.
---@return boolean
local function is_enabled()
	local config = require("sakuin.config").get()
	return not config.progress or config.progress.enabled ~= false
end

--- Called when indexing starts.
---@param label string e.g. "Syncing", "Rebuilding"
function M.start(label)
	M.is_indexing = true
	if is_enabled() then
		vim.notify("[sakuin] " .. label .. "…", vim.log.levels.INFO)
	end
end

--- Called when indexing finishes successfully.
---@param message? string
function M.done(message)
	M.is_indexing = false
	if is_enabled() then
		vim.notify("[sakuin] " .. (message or "done"), vim.log.levels.INFO)
	end
end

--- Called when indexing fails.
---@param label string
---@param err string
function M.fail(label, err)
	M.is_indexing = false
	-- Always notify on errors regardless of enabled setting
	vim.notify("[sakuin] " .. label .. " failed: " .. err, vim.log.levels.ERROR)
end

return M
