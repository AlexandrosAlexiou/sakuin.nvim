local M = {}

---@return string
local function plugin_root()
	local source = debug.getinfo(1, "S").source:sub(2)
	return vim.fn.fnamemodify(source, ":h:h:h")
end

---@return string path, string lib_name
local function lib_path()
	local root = plugin_root()
	local os_name = jit.os
	local prefix, ext

	if os_name == "Windows" then
		prefix = ""
		ext = ".dll"
	elseif os_name == "OSX" then
		prefix = "lib"
		ext = ".dylib"
	else
		prefix = "lib"
		ext = ".so"
	end

	local name = prefix .. "sakuin" .. ext
	return root .. "/build/" .. name, name
end

---@return boolean
function M.has_binary()
	local path = lib_path()
	return vim.fn.filereadable(path) == 1
end

---@param version? string Tag name (e.g. "v0.1.0"), nil for latest
---@return boolean success
---@return string? error
local function try_download(version)
	local root = plugin_root()
	local download_script = root .. "/scripts/download.lua"

	if vim.fn.filereadable(download_script) == 0 then
		return false, "download script not found at " .. download_script
	end

	local download = dofile(download_script)
	return download.download(version)
end

---@return boolean success
---@return string? error
local function try_cargo_build()
	local root = plugin_root()
	local build_script = root .. "/scripts/build.sh"

	if vim.fn.executable("cargo") == 0 then
		return false, "cargo not found in PATH"
	end

	if vim.fn.filereadable(build_script) == 1 then
		print("[sakuin] Building from source with scripts/build.sh ...")
		local result = os.execute("bash " .. vim.fn.shellescape(build_script))
		local ok = (result == 0 or result == true)
		if ok then
			return true, nil
		else
			return false, "build.sh failed"
		end
	end

	-- Fallback: direct cargo build
	print("[sakuin] Building from source with cargo ...")
	local rust_dir = root .. "/rust"
	local cmd = string.format("cargo build --manifest-path %s/Cargo.toml --release", vim.fn.shellescape(rust_dir))

	local result = os.execute(cmd)
	local ok = (result == 0 or result == true)
	if not ok then
		return false, "cargo build failed"
	end

	local _, lib_name = lib_path()
	local build_dir = root .. "/build"
	vim.fn.mkdir(build_dir, "p")

	local src = rust_dir .. "/target/release/" .. lib_name
	local dest = build_dir .. "/" .. lib_name
	local copy_ok = vim.loop.fs_copyfile(src, dest)
	if not copy_ok then
		return false, "failed to copy " .. src .. " to " .. dest
	end

	return true, nil
end

-- Tries: 1) check if it exists, 2) download prebuilt, 3) build from source.
-- Used as the lazy.nvim build hook.
---@param opts? { version?: string }
function M.ensure_binary(opts)
	opts = opts or {}

	if M.has_binary() then
		print("[sakuin] Native library already present.")
		return
	end

	print("[sakuin] Native library not found. Attempting download ...")
	local dl_ok, dl_err = try_download(opts.version)
	if dl_ok then
		print("[sakuin] Prebuilt binary installed successfully.")
		return
	end

	print("[sakuin] Download failed: " .. (dl_err or "unknown error"))
	print("[sakuin] Falling back to building from source ...")

	local build_ok, build_err = try_cargo_build()
	if build_ok then
		print("[sakuin] Built from source successfully.")
		return
	end

	local msg = string.format(
		"[sakuin] Failed to obtain native library.\n"
			.. "  Download error: %s\n"
			.. "  Build error: %s\n"
		.. "  Install Rust (https://rustup.rs) and run scripts/build.sh, "
		.. "or download a binary from GitHub Releases.",
		dl_err or "unknown",
		build_err or "unknown"
	)
	vim.notify(msg, vim.log.levels.ERROR)
end

return M
