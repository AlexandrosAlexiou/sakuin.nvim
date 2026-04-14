--- sakuin.nvim — Health check for :checkhealth sakuin

local M = {}

---@param bytes number
---@return string
local function format_bytes(bytes)
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

function M.check()
	vim.health.start("sakuin.nvim")

	-- Check Neovim version
	if vim.fn.has("nvim-0.10") == 1 then
		vim.health.ok("Neovim >= 0.10")
	else
		vim.health.error("Neovim >= 0.10 is required")
	end

	-- Check LuaJIT
	if jit then
		vim.health.ok("LuaJIT " .. jit.version)
	else
		vim.health.error("LuaJIT is required (standard Lua is not supported)")
	end

	-- Check native library
	local ffi_ok, ffi_mod = pcall(require, "sakuin.ffi")
	if not ffi_ok then
		vim.health.error("Failed to load sakuin.ffi module", { "Check plugin installation" })
		return
	end

	local load_ok, load_err = pcall(ffi_mod.load)
	if load_ok then
		vim.health.ok("Native library loaded")
	else
		vim.health.error("Native library not found: " .. tostring(load_err), {
			"Run :SakuinBuild to compile from source (requires Rust toolchain)",
			"Or download a prebuilt binary from GitHub Releases",
		})
	end

	-- Check snacks.nvim (picker dependency)
	local has_snacks = pcall(require, "snacks")
	if has_snacks then
		vim.health.ok("snacks.nvim available")
	else
		vim.health.warn("snacks.nvim not found", {
			"Install snacks.nvim for the search picker UI",
		})
	end

	-- Check Rust toolchain (informational)
	if vim.fn.executable("cargo") == 1 then
		local handle = io.popen("cargo --version 2>&1")
		if handle then
			local version = handle:read("*l") or "unknown"
			handle:close()
			vim.health.ok("Rust toolchain: " .. version)
		end
	else
		vim.health.info("Rust toolchain not found (only needed for building from source)")
	end

	-- Check index stats (only if library is loaded)
	if load_ok and ffi_mod.is_loaded() then
		local stats = ffi_mod.stats()
		if stats then
			vim.health.ok(
				string.format(
					"Index: %d files, %d segments, %s on disk",
					stats.num_docs,
					stats.num_segments,
					format_bytes(stats.index_size_bytes)
				)
			)
		else
			vim.health.info("No index found (run :SakuinBuild to create one)")
		end
	end
end

return M
