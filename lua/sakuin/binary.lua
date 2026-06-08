--- Native library version, naming, and path resolution.
---
--- Installs write a versioned filename (`libsakuin_<ver>.<ext>`) so a version
--- bump no longer matches the file on disk and re-triggers the install. The
--- unversioned name is legacy (pre-versioning downloads); it's a load-time
--- fallback only, never "current", and is cleaned up once the versioned binary
--- lands.

local M = {}

--- Single source of truth for the version. Bump alongside `rust/Cargo.toml`.
--- The release tag is `"v" .. M.version`.
M.version = "0.6.1"

---@return string
local function plugin_root()
	local source = debug.getinfo(1, "S").source:sub(2)
	return vim.fn.fnamemodify(source, ":h:h:h")
end
M.plugin_root = plugin_root

---@return string ext, string prefix
local function lib_affixes()
	local exts = { Windows = ".dll", OSX = ".dylib" }
	local ext = exts[jit.os] or ".so"
	local prefix = jit.os == "Windows" and "" or "lib"
	return ext, prefix
end

---@return string
function M.unversioned_name()
	local ext, prefix = lib_affixes()
	return prefix .. "sakuin" .. ext
end

---@param version? string defaults to the current version
---@return string
function M.versioned_name(version)
	local ext, prefix = lib_affixes()
	return prefix .. "sakuin_" .. (version or M.version) .. ext
end

---@return string
function M.build_dir() return plugin_root() .. "/build" end

---@return string
function M.unversioned_path() return M.build_dir() .. "/" .. M.unversioned_name() end

---@param version? string
---@return string
function M.versioned_path(version) return M.build_dir() .. "/" .. M.versioned_name(version) end

local function readable(path) return vim.fn.filereadable(path) == 1 end

--- Returns the current versioned path even when absent, so callers can surface
--- the expected location in error messages.
---@return string path
---@return boolean readable
function M.resolve()
	local current = M.versioned_path()
	if readable(current) then return current, true end
	local legacy = M.unversioned_path()
	if readable(legacy) then return legacy, true end
	return current, false
end

---@return boolean
function M.is_current() return readable(M.versioned_path()) end

---@return string[]
function M.installed_versions()
	local ext, prefix = lib_affixes()
	local pat = "^" .. prefix .. "sakuin_(.+)" .. ext:gsub("%.", "%%.") .. "$"
	local uv = vim.uv or vim.loop
	local versions = {}
	local handle = uv.fs_scandir(M.build_dir())
	if not handle then return versions end
	while true do
		local name = uv.fs_scandir_next(handle)
		if not name then break end
		local v = name:match(pat)
		if v then versions[#versions + 1] = v end
	end
	return versions
end

--- No-op until the current binary exists, so the legacy fallback survives a
--- failed install.
function M.cleanup_stale()
	if not readable(M.versioned_path()) then return end
	for _, v in ipairs(M.installed_versions()) do
		if v ~= M.version then os.remove(M.versioned_path(v)) end
	end
	local legacy = M.unversioned_path()
	if readable(legacy) then os.remove(legacy) end
end

return M
