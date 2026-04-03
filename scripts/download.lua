#!/usr/bin/env -S nvim -l
--- sakuin.nvim — Download prebuilt binary from GitHub Releases.
--- Can be run as: nvim -l scripts/download.lua
--- Or called programmatically: dofile("scripts/download.lua")

local M = {}

--- GitHub repository
M.repo = "AlexandrosAlexiou/sakuin.nvim"

--- Map of (os, arch) to the artifact filename on GitHub Releases.
M.artifacts = {
	["Linux-x64"] = "libsakuin-x86_64-linux.so",
	["Linux-arm64"] = "libsakuin-aarch64-linux.so",
	["OSX-arm64"] = "libsakuin-aarch64-darwin.dylib",
	["OSX-x64"] = "libsakuin-x86_64-darwin.dylib",
	["Windows-x64"] = "sakuin-x86_64-windows.dll",
}

--- Map artifact names to the local filename they should be saved as.
M.local_names = {
	["Linux-x64"] = "libsakuin.so",
	["Linux-arm64"] = "libsakuin.so",
	["OSX-arm64"] = "libsakuin.dylib",
	["OSX-x64"] = "libsakuin.dylib",
	["Windows-x64"] = "sakuin.dll",
}

--- Detect the current platform key.
---@return string|nil key e.g. "OSX-arm64", or nil if unsupported
function M.detect_platform()
	local os_name = jit and jit.os or nil
	local arch = jit and jit.arch or nil

	if not os_name or not arch then
		return nil
	end

	-- jit.arch returns "x64", "x86", "arm64", "arm", "ppc", "mips"
	local key = os_name .. "-" .. arch
	if M.artifacts[key] then
		return key
	end

	return nil
end

--- Get the plugin root directory.
---@return string
function M.plugin_root()
	local source = debug.getinfo(1, "S").source:sub(2) -- strip leading @
	return vim.fn.fnamemodify(source, ":h:h")
end

--- Download the prebuilt binary for the current platform.
---@param version? string Tag name (e.g. "v0.1.0"). nil = latest.
---@return boolean success
---@return string? error
function M.download(version)
	local platform = M.detect_platform()
	if not platform then
		return false, "Unsupported platform: " .. (jit and (jit.os .. "-" .. jit.arch) or "unknown")
	end

	local artifact = M.artifacts[platform]
	local local_name = M.local_names[platform]
	local root = M.plugin_root()
	local build_dir = root .. "/build"
	local dest = build_dir .. "/" .. local_name

	-- Ensure build directory exists
	vim.fn.mkdir(build_dir, "p")

	-- Determine download URL
	local url
	if version then
		url = string.format("https://github.com/%s/releases/download/%s/%s", M.repo, version, artifact)
	else
		url = string.format("https://github.com/%s/releases/latest/download/%s", M.repo, artifact)
	end

	-- Download using curl (available on all platforms)
	print("[sakuin] Downloading " .. artifact .. " ...")
	local cmd = string.format("curl -fSL --create-dirs -o %s %s", vim.fn.shellescape(dest), vim.fn.shellescape(url))

	local result = os.execute(cmd)
	-- os.execute returns different types depending on Lua version
	local ok = (result == 0 or result == true)

	if not ok then
		return false, "Download failed. URL: " .. url
	end

	-- Make executable on Unix
	if jit.os ~= "Windows" then
		os.execute("chmod +x " .. vim.fn.shellescape(dest))
	end

	print("[sakuin] Downloaded to " .. dest)
	return true, nil
end

-- If run directly (nvim -l scripts/download.lua), execute download
if arg and arg[0] and arg[0]:match("download%.lua$") then
	local ok, err = M.download(arg[1]) -- optional version argument
	if not ok then
		io.stderr:write("[sakuin] Error: " .. (err or "unknown") .. "\n")
		os.exit(1)
	end
end

return M
