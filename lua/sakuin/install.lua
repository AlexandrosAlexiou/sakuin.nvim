local binary = require("sakuin.binary")

local M = {}

---@return string
local function plugin_root() return binary.plugin_root() end

---@return boolean
function M.has_binary() return binary.is_current() end

local proc = require("sakuin.proc")

local install_in_progress = false
---@type fun(ok: boolean)[]
local install_waiters = {}

---@return boolean
function M.is_installing() return install_in_progress end

--- Build the native library from source.
---@param run fun(cmd: string[], opts?: table): vim.SystemCompleted
---@return boolean ok
---@return string? err
local function build_impl(run)
	local root = plugin_root()
	local rust_dir = root .. "/rust"
	if vim.fn.executable("cargo") == 0 then return false, "cargo not found in PATH" end

	vim.fn.mkdir(root .. "/build", "p")
	local log_path = root .. "/build/build.log"
	local log = io.open(log_path, "w")

	-- Stream cargo's latest progress line to the cmdline while logging in full.
	local pending = ""
	local function on_chunk(_, data)
		if not data then return end
		if log then log:write(data) end
		pending = pending .. data
		if not pending:find("\n", 1, true) then return end
		local lines = vim.split(pending, "\n", { plain = true })
		pending = lines[#lines]
		local latest
		for i = #lines - 1, 1, -1 do
			local trimmed = lines[i]:gsub("\r", ""):match("^%s*(.-)%s*$")
			if trimmed ~= "" then
				latest = trimmed
				break
			end
		end
		if latest then vim.schedule(function() vim.api.nvim_echo({ { "[sakuin] " .. latest, "MoreMsg" } }, false, {}) end) end
	end

	local result = run({ "cargo", "build", "--manifest-path", rust_dir .. "/Cargo.toml", "--release", "--lib" }, {
		text = true,
		stdout = on_chunk,
		stderr = on_chunk,
	})
	if log then log:close() end
	if result.code ~= 0 then return false, "build failed (log: " .. log_path .. ")" end

	local src = rust_dir .. "/target/release/" .. binary.unversioned_name()
	local dest = binary.versioned_path()
	if vim.fn.filereadable(dest) == 1 then os.remove(dest) end
	if not (vim.uv or vim.loop).fs_copyfile(src, dest) then return false, "failed to copy " .. src .. " to " .. dest end
	-- Ad-hoc re-sign on macOS so the kernel accepts the freshly written dylib.
	if jit.os == "OSX" then run({ "codesign", "-s", "-", "-f", dest }) end
	return true
end

--- Try the prebuilt download, fall back to building from source.
---@param run fun(cmd: string[], opts?: table): vim.SystemCompleted
---@param opts { version?: string }
---@return boolean ok
---@return string? err
local function install_impl(run, opts)
	local download_script = plugin_root() .. "/scripts/download.lua"
	if vim.fn.filereadable(download_script) == 0 then return false, "download script not found at " .. download_script end

	vim.notify("Downloading prebuilt binary…", vim.log.levels.INFO, { title = "sakuin" })
	local ok, err = dofile(download_script).download_impl(run, opts.version)
	if ok then return true end

	vim.notify(
		"Prebuilt download failed (" .. (err or "unknown") .. ") — building from source…",
		vim.log.levels.WARN,
		{ title = "sakuin" }
	)
	return build_impl(run)
end

---@param opts? { version?: string }
---@param on_ready? fun(ok: boolean)
function M.ensure_binary(opts, on_ready)
	opts = opts or {}
	on_ready = on_ready or function() end

	if M.has_binary() then
		-- Sweep leftovers from a prior interrupted download (orphan .tmp, old sidecars).
		binary.cleanup_stale()
		on_ready(true)
		return
	end

	-- Single-flight: callers that arrive mid-install join the in-flight one.
	install_waiters[#install_waiters + 1] = on_ready
	if install_in_progress then return end
	install_in_progress = true

	proc.drive_async(
		install_impl,
		vim.schedule_wrap(function(ok, err)
			install_in_progress = false
			if ok then
				binary.cleanup_stale()
				vim.notify("Native library installed.", vim.log.levels.INFO, { title = "sakuin" })
			else
				vim.notify("Native library install failed: " .. (err or "unknown"), vim.log.levels.ERROR, { title = "sakuin" })
			end
			local waiters = install_waiters
			install_waiters = {}
			for _, callback in ipairs(waiters) do
				callback(ok)
			end
		end),
		opts
	)
end

--- Blocking install for lazy's `build` step, so lazy waits and sees failures.
---@param opts? { version?: string }
function M.build(opts)
	if require("sakuin.ffi").is_loaded() then
		vim.schedule(
			function() vim.notify("Restart Neovim to load the update.", vim.log.levels.WARN, { title = "sakuin" }) end
		)
	end
	if M.has_binary() then
		binary.cleanup_stale()
		return
	end
	local ok, err = proc.drive_sync(install_impl, opts or {})
	if not ok then error("[sakuin] native binary install failed: " .. (err or "unknown")) end
	binary.cleanup_stale()
end

return M
