#!/usr/bin/env -S nvim -l
--- sakuin.nvim — Download prebuilt binary from GitHub Releases.
--- Can be run as: nvim -l scripts/download.lua
--- Or called programmatically: dofile("scripts/download.lua")

-- Make `require("sakuin.binary")` resolve when run standalone via `nvim -l`.
do
	local source = debug.getinfo(1, "S").source:sub(2)
	local root = vim.fn.fnamemodify(source, ":h:h")
	package.path = root .. "/lua/?.lua;" .. package.path
end
local binary = require("sakuin.binary")

local M = {}

M.repo = "AlexandrosAlexiou/sakuin.nvim"

M.version = "v" .. binary.version

-- target triple → release artifact filename
-- Convention: `lib<name>-<triple>.{so,dylib}` on unix, `<name>-<triple>.dll` on Windows.
M.artifacts = {
	["aarch64-apple-darwin"] = "libsakuin-aarch64-apple-darwin.dylib",
	["x86_64-apple-darwin"] = "libsakuin-x86_64-apple-darwin.dylib",
	["aarch64-pc-windows-msvc"] = "sakuin-aarch64-pc-windows-msvc.dll",
	["x86_64-pc-windows-msvc"] = "sakuin-x86_64-pc-windows-msvc.dll",
	["aarch64-unknown-linux-gnu"] = "libsakuin-aarch64-unknown-linux-gnu.so",
	["x86_64-unknown-linux-gnu"] = "libsakuin-x86_64-unknown-linux-gnu.so",
	["aarch64-unknown-linux-musl"] = "libsakuin-aarch64-unknown-linux-musl.so",
	["x86_64-unknown-linux-musl"] = "libsakuin-x86_64-unknown-linux-musl.so",
	["aarch64-unknown-freebsd"] = "libsakuin-aarch64-unknown-freebsd.so",
	["x86_64-unknown-freebsd"] = "libsakuin-x86_64-unknown-freebsd.so",
	["aarch64-unknown-openbsd"] = "libsakuin-aarch64-unknown-openbsd.so",
	["x86_64-unknown-openbsd"] = "libsakuin-x86_64-unknown-openbsd.so",
	["aarch64-linux-android"] = "libsakuin-aarch64-linux-android.so",
}

local function file_exists(path)
	local f = io.open(path, "r")
	if f then
		f:close()
		return true
	end
	return false
end

---@return "musl"|"gnu"
local function detect_libc()
	if file_exists("/etc/alpine-release") then return "musl" end
	for _, p in ipairs({
		"/lib/ld-musl-x86_64.so.1",
		"/lib/ld-musl-aarch64.so.1",
	}) do
		if file_exists(p) then return "musl" end
	end
	-- ldd on musl prints "musl libc"; on glibc prints "GNU libc" / "GLIBC"
	local handle = io.popen("ldd --version 2>&1")
	if handle then
		local out = handle:read("*a") or ""
		handle:close()
		if out:lower():find("musl") then return "musl" end
	end
	return "gnu"
end

local function detect_android()
	if os.getenv("ANDROID_ROOT") or os.getenv("ANDROID_DATA") then return true end
	return file_exists("/system/build.prop")
end

---@return string|nil "x86_64" | "aarch64"
local function detect_arch()
	local handle = io.popen("uname -m 2>/dev/null")
	if handle then
		local out = handle:read("*l")
		handle:close()
		if out then
			out = out:lower()
			if out == "x86_64" or out == "amd64" then return "x86_64" end
			if out == "arm64" or out == "aarch64" then return "aarch64" end
		end
	end
	if jit and jit.arch then
		if jit.arch == "x64" then return "x86_64" end
		if jit.arch == "arm64" then return "aarch64" end
	end
	return nil
end

---@return string|nil triple e.g. "x86_64-unknown-linux-gnu"
function M.detect_triple()
	local uname = (vim.uv or vim.loop).os_uname()
	local sys = uname.sysname or ""
	local arch = detect_arch()
	if not arch then return nil end

	if sys == "Darwin" then return arch .. "-apple-darwin" end
	if sys == "Windows_NT" or sys:match("^MINGW") or sys:match("^MSYS") or sys:match("^CYGWIN") then
		return arch .. "-pc-windows-msvc"
	end
	if sys == "Linux" then
		if detect_android() and arch == "aarch64" then return "aarch64-linux-android" end
		return arch .. "-unknown-linux-" .. detect_libc()
	end
	if sys == "FreeBSD" then return arch .. "-unknown-freebsd" end
	if sys == "OpenBSD" then return arch .. "-unknown-openbsd" end
	return nil
end

local function curl_args(url, dest)
	return {
		"curl",
		"--fail",
		"--location",
		"--silent",
		"--show-error",
		"--create-dirs",
		"--output",
		dest,
		url,
	}
end

---@param result vim.SystemCompleted
---@return boolean ok
---@return string? error
local function curl_result(result)
	if result.code ~= 0 then
		local stderr = (result.stderr or ""):gsub("^%s+", ""):gsub("%s+$", "")
		if stderr == "" then stderr = "curl exited with code " .. tostring(result.code) end
		return false, stderr
	end
	return true, nil
end

---@param triple string
---@param path string
---@return string[]
local function sha256_cmd(triple, path)
	if triple:find("apple%-darwin") then return { "shasum", "-a", "256", path } end
	if triple:find("windows") then return { "certutil", "-hashfile", path, "SHA256" } end
	if triple:find("openbsd") then return { "sha256", path } end
	-- linux (incl. android), freebsd
	return { "sha256sum", path }
end

---@param triple string
---@param stdout string
---@return string?
local function parse_sha256_stdout(triple, stdout)
	if triple:find("windows") then
		-- certutil prints a header line, then hex (occasionally space-separated bytes), then a footer.
		for line in stdout:gmatch("[^\r\n]+") do
			local hex = line:gsub("%s", ""):lower()
			if hex:match("^%x+$") and #hex == 64 then return hex end
		end
		return nil
	end
	if triple:find("openbsd") then
		-- "SHA256 (file) = <hex>"
		local hex = stdout:match("=%s*(%x+)")
		return hex and hex:lower() or nil
	end
	-- "<hex>  <filename>"
	local hex = stdout:match("^(%x+)")
	return hex and hex:lower() or nil
end

---@param sidecar_path string
---@return string? hex
---@return string? error
local function read_expected_sha(sidecar_path)
	local f = io.open(sidecar_path, "r")
	if not f then return nil, "could not open sidecar " .. sidecar_path end
	local content = f:read("*a") or ""
	f:close()
	local hex = content:match("(%x+)")
	if not hex or #hex ~= 64 then return nil, "sidecar empty or malformed: " .. sidecar_path end
	return hex:lower(), nil
end

---@param run fun(cmd: string[], opts?: table): vim.SystemCompleted
---@param triple string
---@param binary_path string
---@param sidecar_path string
---@return boolean ok
---@return string? err
local function verify_checksum(run, triple, binary_path, sidecar_path)
	local expected, read_err = read_expected_sha(sidecar_path)
	if not expected then return false, read_err end

	local cmd = sha256_cmd(triple, binary_path)
	if vim.fn.executable(cmd[1]) == 0 then
		vim.notify("[sakuin] " .. cmd[1] .. " not found; skipping checksum verification", vim.log.levels.WARN)
		return true
	end

	local result = run(cmd)
	if result.code ~= 0 then
		local stderr = (result.stderr or ""):gsub("%s+$", "")
		return false, "hash command failed: " .. (stderr ~= "" and stderr or ("exit " .. result.code))
	end

	local actual = parse_sha256_stdout(triple, result.stdout or "")
	if not actual then return false, "could not parse hash from `" .. cmd[1] .. "` output" end
	if expected ~= actual then return false, string.format("expected %s, got %s", expected, actual) end
	return true
end

--- Download and verify the prebuilt binary.
---@param run fun(cmd: string[], opts?: table): vim.SystemCompleted
---@param version? string
---@return boolean ok
---@return string? err
function M.download_impl(run, version)
	local triple = M.detect_triple()
	if not triple or not M.artifacts[triple] then return false, "Unsupported platform: " .. (triple or "unknown") end

	local build_dir = binary.build_dir()
	local dest = binary.versioned_path()
	local dest_tmp = dest .. ".tmp"
	local sidecar = dest .. ".sha256"
	vim.fn.mkdir(build_dir, "p")

	local base = string.format("https://github.com/%s/releases/download/%s", M.repo, version or M.version)
	local lib_url = base .. "/" .. M.artifacts[triple]

	local ok, err = curl_result(run(curl_args(lib_url, dest_tmp)))
	if not ok then
		os.remove(dest_tmp)
		return false, "download failed (" .. lib_url .. "): " .. (err or "unknown")
	end

	ok, err = curl_result(run(curl_args(lib_url .. ".sha256", sidecar)))
	if not ok then
		os.remove(dest_tmp)
		os.remove(sidecar)
		return false, "sidecar download failed (" .. lib_url .. ".sha256): " .. (err or "unknown")
	end

	ok, err = verify_checksum(run, triple, dest_tmp, sidecar)
	if not ok then
		os.remove(dest_tmp)
		os.remove(sidecar)
		return false, "checksum verification failed: " .. (err or "unknown")
	end

	local rn_ok, rn_err = (vim.uv or vim.loop).fs_rename(dest_tmp, dest)
	if not rn_ok then
		os.remove(dest_tmp)
		return false, "rename failed: " .. tostring(rn_err or "unknown")
	end

	if not triple:find("windows") then
		run({ "chmod", "+x", dest })
		-- Strip the macOS quarantine attribute so the kernel accepts the dylib.
		if triple:find("apple%-darwin") then run({ "xattr", "-d", "com.apple.quarantine", dest }) end
	end
	return true
end

if arg and arg[0] and arg[0]:match("download%.lua$") then
	local ok, err = require("sakuin.proc").drive_sync(M.download_impl, arg[1])
	if not ok then
		io.stderr:write("[sakuin] Error: " .. (err or "unknown") .. "\n")
		os.exit(1)
	end
end

return M
