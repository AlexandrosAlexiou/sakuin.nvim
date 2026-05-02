local M = {}

M.is_indexing = false

local function is_enabled()
	local config = require("sakuin.config").get()
	return not config.progress or config.progress.enabled ~= false
end

---@param label string
function M.start(label)
	M.is_indexing = true
	if is_enabled() then
		vim.notify("[sakuin] " .. label .. "…", vim.log.levels.INFO)
	end
end

---@param message? string
function M.done(message)
	M.is_indexing = false
	if is_enabled() then
		vim.notify("[sakuin] " .. (message or "done"), vim.log.levels.INFO)
	end
end

-- Errors notify regardless of progress.enabled.
---@param label string
---@param err string
function M.fail(label, err)
	M.is_indexing = false
	vim.notify("[sakuin] " .. label .. " failed: " .. err, vim.log.levels.ERROR)
end

return M
