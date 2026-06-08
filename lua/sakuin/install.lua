local binary = require("sakuin.binary")

local M = {}

---@return string
local function plugin_root() return binary.plugin_root() end

---@return boolean
function M.has_binary() return binary.is_current() end

---@param version? string
---@param on_done fun(ok: boolean, err?: string)
local function try_download_async(version, on_done)
	local root = plugin_root()
	local download_script = root .. "/scripts/download.lua"

	if vim.fn.filereadable(download_script) == 0 then
		on_done(false, "download script not found at " .. download_script)
		return
	end

	local download = dofile(download_script)
	download.download(version, on_done)
end

local build_in_progress = false
---@type fun(ok: boolean)[]
local build_pending = {}

local BUILD_NOTIFY_ID = "sakuin-build"

local download_in_progress = false
---@type fun(ok: boolean)[]
local download_pending = {}

---@return boolean
function M.is_installing() return download_in_progress or build_in_progress end

---@param on_done fun(ok: boolean, err?: string)
local function build_async(on_done)
	local root = plugin_root()
	local rust_dir = root .. "/rust"

	if vim.fn.executable("cargo") == 0 then
		on_done(false, "cargo not found in PATH")
		return
	end

	local log_path = root .. "/build/build.log"
	vim.fn.mkdir(root .. "/build", "p")
	local log = io.open(log_path, "w")

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
		if not latest then return end

		vim.schedule(function() vim.api.nvim_echo({ { "[sakuin] " .. latest, "MoreMsg" } }, false, {}) end)
	end

	local cmd = { "cargo", "build", "--manifest-path", rust_dir .. "/Cargo.toml", "--release", "--lib" }

	vim.system(
		cmd,
		{
			text = true,
			stdout = on_chunk,
			stderr = on_chunk,
		},
		vim.schedule_wrap(function(result)
			if log then log:close() end

			if result.code ~= 0 then
				on_done(false, "build failed (log: " .. log_path .. ")")
				return
			end

			local src = rust_dir .. "/target/release/" .. binary.unversioned_name()
			local dest = binary.versioned_path()
			if vim.fn.filereadable(dest) == 1 then os.remove(dest) end
			local copy_ok = (vim.uv or vim.loop).fs_copyfile(src, dest)
			if not copy_ok then
				on_done(false, "failed to copy " .. src .. " to " .. dest)
				return
			end

			-- Ad-hoc re-sign on macOS so the kernel accepts the freshly written dylib.
			if jit.os == "OSX" then vim.system({ "codesign", "-s", "-", "-f", dest }, { text = true }):wait() end

			binary.cleanup_stale()
			on_done(true)
		end)
	)
end

---@param on_ready fun(ok: boolean)
local function start_or_join_build(on_ready)
	if build_in_progress then
		build_pending[#build_pending + 1] = on_ready
		return
	end
	build_in_progress = true

	vim.notify("Building from source…", vim.log.levels.INFO, { id = BUILD_NOTIFY_ID, title = "sakuin" })

	build_async(function(ok, err)
		build_in_progress = false
		if ok then
			vim.notify("Built from source successfully.", vim.log.levels.INFO, { id = BUILD_NOTIFY_ID, title = "sakuin" })
		else
			vim.notify(
				"Build from source failed: " .. (err or "unknown"),
				vim.log.levels.ERROR,
				{ id = BUILD_NOTIFY_ID, title = "sakuin" }
			)
		end
		local waiters = build_pending
		build_pending = {}
		on_ready(ok)
		for _, callback in ipairs(waiters) do
			callback(ok)
		end
	end)
end

---@param opts? { version?: string }
---@param on_ready? fun(ok: boolean)
function M.ensure_binary(opts, on_ready)
	opts = opts or {}
	on_ready = on_ready or function() end

	if M.has_binary() then
		on_ready(true)
		return
	end

	if build_in_progress then
		build_pending[#build_pending + 1] = on_ready
		return
	end

	if download_in_progress then
		download_pending[#download_pending + 1] = on_ready
		return
	end

	download_in_progress = true
	vim.notify("Downloading prebuilt binary…", vim.log.levels.INFO, { title = "sakuin" })

	try_download_async(
		opts.version,
		vim.schedule_wrap(function(dl_ok, dl_err)
			download_in_progress = false
			local waiters = download_pending
			download_pending = {}

			if dl_ok then
				binary.cleanup_stale()
				vim.notify("Prebuilt binary installed.", vim.log.levels.INFO, { title = "sakuin" })
				on_ready(true)
				for _, callback in ipairs(waiters) do
					callback(true)
				end
				return
			end

			vim.notify("Prebuilt download failed: " .. (dl_err or "unknown"), vim.log.levels.WARN, { title = "sakuin" })

			start_or_join_build(on_ready)
			for _, callback in ipairs(waiters) do
				start_or_join_build(callback)
			end
		end)
	)
end

return M
