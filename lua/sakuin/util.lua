local M = {}

---@param bytes number
---@return string
function M.format_bytes(bytes)
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

return M
