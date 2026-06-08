#!/usr/bin/env -S nvim -l
--- sakuin.nvim — Download prebuilt binary from GitHub Releases.
--- Can be run as: nvim -l scripts/download.lua
--- Or called programmatically: dofile("scripts/download.lua")

local M = {}

M.repo = "AlexandrosAlexiou/sakuin.nvim"

-- Tag of the release whose binaries match this checkout. Bump on each
-- release so downloads always target the artifacts produced for the code
-- currently on disk, not whatever "latest" happens to point at.
M.version = "v0.6.1"

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
	if triple:find("apple%-darwin$") then return "libsakuin.dylib" end
	if triple:find("windows") then return "sakuin.dll" end
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

---@return string
function M.plugin_root()
	local source = debug.getinfo(1, "S").source:sub(2)
	return vim.fn.fnamemodify(source, ":h:h")
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

---@param url string
---@param dest string
---@param on_done fun(ok: boolean, err?: string)
local function curl_async(url, dest, on_done)
	vim.system(curl_args(url, dest), { text = true }, vim.schedule_wrap(function(result) on_done(curl_result(result)) end))
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

---@param triple string
---@param binary_path string
---@param sidecar_path string
---@param on_done fun(ok: boolean, err?: string)
local function verify_checksum_async(triple, binary_path, sidecar_path, on_done)
	local expected, read_err = read_expected_sha(sidecar_path)
	if not expected then
		on_done(false, read_err)
		return
	end

	local cmd = sha256_cmd(triple, binary_path)
	if vim.fn.executable(cmd[1]) == 0 then
		vim.notify("[sakuin] " .. cmd[1] .. " not found; skipping checksum verification", vim.log.levels.WARN)
		on_done(true, nil)
		return
	end

	vim.system(
		cmd,
		{ text = true },
		vim.schedule_wrap(function(result)
			if result.code ~= 0 then
				local stderr = (result.stderr or ""):gsub("%s+$", "")
				on_done(false, "hash command failed: " .. (stderr ~= "" and stderr or ("exit " .. result.code)))
				return
			end

			local actual = parse_sha256_stdout(triple, result.stdout or "")
			if not actual then
				on_done(false, "could not parse hash from `" .. cmd[1] .. "` output")
				return
			end

			if expected ~= actual then
				on_done(false, string.format("expected %s, got %s", expected, actual))
				return
			end
			on_done(true, nil)
		end)
	)
end

---@param version? string
---@param on_done fun(ok: boolean, err?: string)
function M.download(version, on_done)
	local triple = M.detect_triple()
	if not triple or not M.artifacts[triple] then
		on_done(false, "Unsupported platform: " .. (triple or "unknown"))
		return
	end

	local artifact = M.artifacts[triple]
	local local_name = local_name_for(triple)
	local root = M.plugin_root()
	local build_dir = root .. "/build"
	local dest = build_dir .. "/" .. local_name
	local dest_tmp = dest .. ".tmp"
	local sidecar = dest .. ".sha256"

	vim.fn.mkdir(build_dir, "p")

	local tag = version or M.version
	local base = string.format("https://github.com/%s/releases/download/%s", M.repo, tag)
	local lib_url = base .. "/" .. artifact
	local sha_url = lib_url .. ".sha256"

	curl_async(lib_url, dest_tmp, function(ok, err)
		if not ok then
			os.remove(dest_tmp)
			on_done(false, "download failed (" .. lib_url .. "): " .. (err or "unknown"))
			return
		end

		curl_async(sha_url, sidecar, function(sha_ok, sha_err)
			if not sha_ok then
				os.remove(dest_tmp)
				os.remove(sidecar)
				on_done(false, "sidecar download failed (" .. sha_url .. "): " .. (sha_err or "unknown"))
				return
			end

			verify_checksum_async(triple, dest_tmp, sidecar, function(v_ok, v_err)
				if not v_ok then
					os.remove(dest_tmp)
					os.remove(sidecar)
					on_done(false, "checksum verification failed: " .. (v_err or "unknown"))
					return
				end

				local uv = vim.uv or vim.loop
				local rn_ok, rn_err = uv.fs_rename(dest_tmp, dest)
				if not rn_ok then
					os.remove(dest_tmp)
					on_done(false, "rename failed: " .. tostring(rn_err or "unknown"))
					return
				end

				local function finish() on_done(true) end

				if not triple:find("windows") then
					vim.system(
						{ "chmod", "+x", dest },
						{ text = true },
						vim.schedule_wrap(function()
							if triple:find("apple%-darwin") then
								vim.system({ "xattr", "-d", "com.apple.quarantine", dest }, { text = true }, vim.schedule_wrap(finish))
							else
								finish()
							end
						end)
					)
				else
					finish()
				end
			end)
		end)
	end)
end

if arg and arg[0] and arg[0]:match("download%.lua$") then
	M.download(arg[1], function(ok, err)
		if not ok then
			io.stderr:write("[sakuin] Error: " .. (err or "unknown") .. "\n")
			os.exit(1)
		end
	end)
end

return M
