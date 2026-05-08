#!/usr/bin/env -S nvim -l
--- sakuin.nvim — Download prebuilt binary from GitHub Releases.
--- Can be run as: nvim -l scripts/download.lua
--- Or called programmatically: dofile("scripts/download.lua")

local M = {}

M.repo = "AlexandrosAlexiou/sakuin.nvim"

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

---@param triple string
---@return string runtime filename Neovim looks for under build/
local function local_name_for(triple)
	if triple:find("apple%-darwin$") then
		return "libsakuin.dylib"
	end
	if triple:find("windows") then
		return "sakuin.dll"
	end
	return "libsakuin.so"
end

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
	if file_exists("/etc/alpine-release") then
		return "musl"
	end
	for _, p in ipairs({
		"/lib/ld-musl-x86_64.so.1",
		"/lib/ld-musl-aarch64.so.1",
	}) do
		if file_exists(p) then
			return "musl"
		end
	end
	-- ldd on musl prints "musl libc"; on glibc prints "GNU libc" / "GLIBC"
	local handle = io.popen("ldd --version 2>&1")
	if handle then
		local out = handle:read("*a") or ""
		handle:close()
		if out:lower():find("musl") then
			return "musl"
		end
	end
	return "gnu"
end

local function detect_android()
	if os.getenv("ANDROID_ROOT") or os.getenv("ANDROID_DATA") then
		return true
	end
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
			if out == "x86_64" or out == "amd64" then
				return "x86_64"
			end
			if out == "arm64" or out == "aarch64" then
				return "aarch64"
			end
		end
	end
	if jit and jit.arch then
		if jit.arch == "x64" then
			return "x86_64"
		end
		if jit.arch == "arm64" then
			return "aarch64"
		end
	end
	return nil
end

---@return string|nil triple e.g. "x86_64-unknown-linux-gnu"
function M.detect_triple()
	local uname = (vim.uv or vim.loop).os_uname()
	local sys = uname.sysname or ""
	local arch = detect_arch()
	if not arch then
		return nil
	end

	if sys == "Darwin" then
		return arch .. "-apple-darwin"
	end
	if sys == "Windows_NT" or sys:match("^MINGW") or sys:match("^MSYS") or sys:match("^CYGWIN") then
		return arch .. "-pc-windows-msvc"
	end
	if sys == "Linux" then
		if detect_android() and arch == "aarch64" then
			return "aarch64-linux-android"
		end
		return arch .. "-unknown-linux-" .. detect_libc()
	end
	if sys == "FreeBSD" then
		return arch .. "-unknown-freebsd"
	end
	if sys == "OpenBSD" then
		return arch .. "-unknown-openbsd"
	end
	return nil
end

---@return string
function M.plugin_root()
	local source = debug.getinfo(1, "S").source:sub(2)
	return vim.fn.fnamemodify(source, ":h:h")
end

---@param version? string Tag name (e.g. "v0.1.0"). nil = latest.
---@return boolean success
---@return string? error
function M.download(version)
	local triple = M.detect_triple()
	if not triple or not M.artifacts[triple] then
		return false, "Unsupported platform: " .. (triple or "unknown")
	end

	local artifact = M.artifacts[triple]
	local local_name = local_name_for(triple)
	local root = M.plugin_root()
	local build_dir = root .. "/build"
	local dest = build_dir .. "/" .. local_name

	vim.fn.mkdir(build_dir, "p")

	local url
	if version then
		url = string.format("https://github.com/%s/releases/download/%s/%s", M.repo, version, artifact)
	else
		url = string.format("https://github.com/%s/releases/latest/download/%s", M.repo, artifact)
	end

	print("[sakuin] Downloading " .. artifact .. " ...")
	local cmd = string.format("curl -fSL --create-dirs -o %s %s", vim.fn.shellescape(dest), vim.fn.shellescape(url))

	local result = os.execute(cmd)
	local ok = (result == 0 or result == true)

	if not ok then
		return false, "Download failed. URL: " .. url
	end

	if not triple:find("windows") then
		os.execute("chmod +x " .. vim.fn.shellescape(dest))
	end
	if triple:find("apple%-darwin") then
		os.execute("xattr -d com.apple.quarantine " .. vim.fn.shellescape(dest) .. " 2>/dev/null")
	end

	print("[sakuin] Downloaded to " .. dest)
	return true, nil
end

-- If run directly (nvim -l scripts/download.lua), execute download
if arg and arg[0] and arg[0]:match("download%.lua$") then
	local ok, err = M.download(arg[1])
	if not ok then
		io.stderr:write("[sakuin] Error: " .. (err or "unknown") .. "\n")
		os.exit(1)
	end
end

return M
