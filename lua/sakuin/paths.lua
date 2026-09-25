--- Where the index for a project lives.
---
--- The index is kept outside the project tree, under Neovim's cache directory,
--- so tantivy's segment writes never show up as filesystem events inside the
--- repo. Writing into `<root>/.sakuin` made every git-aware tool watching the
--- repo (explorers, statuslines, shell prompts) re-run `git status` on each
--- segment flush, which on large repos snowballed into a storm of git
--- processes. The directory name follows Neovim's own `undodir` convention:
--- the absolute root with `/` replaced by `%`, so it stays readable and
--- collision-free without hashing.

local M = {}

---@param root string absolute project root
---@return string
function M.index_dir(root)
	local name = root:gsub("/", "%%")
	local dir = vim.fn.stdpath("cache") .. "/sakuin/" .. name
	M.migrate_legacy(root, dir)
	return dir
end

--- Older versions wrote the index to `<root>/.sakuin`. Move it to the new
--- location once so the project auto-syncs on startup instead of looking
--- unindexed. Idempotent; a failed rename leaves the legacy dir in place.
---@param root string
---@param dir string
function M.migrate_legacy(root, dir)
	local legacy = root .. "/.sakuin"
	if vim.fn.isdirectory(legacy) == 0 or vim.fn.isdirectory(dir) == 1 then return end
	vim.fn.mkdir(vim.fn.fnamemodify(dir, ":h"), "p")
	local ok = vim.uv.fs_rename(legacy, dir)
	if ok then
		vim.notify("[sakuin] moved index from " .. legacy .. " to " .. dir, vim.log.levels.INFO, { title = "sakuin" })
	else
		vim.notify(
			"[sakuin] index now lives at " .. dir .. "; the old " .. legacy .. " can be deleted",
			vim.log.levels.WARN,
			{ title = "sakuin" }
		)
	end
end

return M
