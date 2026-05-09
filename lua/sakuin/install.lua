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
	local exts = { Windows = ".dll", OSX = ".dylib" }
	local ext = exts[os_name] or ".so"
	local prefix = os_name == "Windows" and "" or "lib"

	local name = prefix .. "sakuin" .. ext
	return root .. "/build/" .. name, name
end

---@return boolean
function M.has_binary()
	local path = lib_path()
	return vim.fn.filereadable(path) == 1
end

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

local download_in_progress = false
---@type fun(ok: boolean)[]
local download_pending = {}

---@return boolean
function M.is_installing()
	return download_in_progress or build_in_progress
end

---@param on_done fun(ok: boolean, err?: string)
local function build_async(on_done)
	local root = plugin_root()
	local build_script = root .. "/scripts/build.sh"

	if vim.fn.executable("cargo") == 0 then
		on_done(false, "cargo not found in PATH")
		return
	end

	local log_path = root .. "/build/build.log"
	vim.fn.mkdir(root .. "/build", "p")
	local log = io.open(log_path, "w")
	local function on_chunk(_, data)
		if log and data then
			log:write(data)
		end
	end

	local cmd
	local needs_copy = false
	if vim.fn.filereadable(build_script) == 1 then
		cmd = { "bash", build_script, "lib" }
	else
		local rust_dir = root .. "/rust"
		cmd = { "cargo", "build", "--manifest-path", rust_dir .. "/Cargo.toml", "--release", "--lib" }
		needs_copy = true
	end

	vim.system(
		cmd,
		{
			text = true,
			stdout = on_chunk,
			stderr = on_chunk,
		},
		vim.schedule_wrap(function(result)
			if log then
				log:close()
			end

			if result.code ~= 0 then
				on_done(false, "build failed (log: " .. log_path .. ")")
				return
			end

			if needs_copy then
				local _, lib_name = lib_path()
				local rust_dir = root .. "/rust"
				local src = rust_dir .. "/target/release/" .. lib_name
				local dest = root .. "/build/" .. lib_name
				local copy_ok = (vim.uv or vim.loop).fs_copyfile(src, dest)
				if not copy_ok then
					on_done(false, "failed to copy " .. src .. " to " .. dest)
					return
				end
			end

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

	vim.notify("Building from source (this may take a few minutes)…", vim.log.levels.INFO, { title = "sakuin" })

	build_async(function(ok, err)
		build_in_progress = false
		if ok then
			vim.notify("Built from source successfully.", vim.log.levels.INFO, { title = "sakuin" })
		else
			vim.notify("Build from source failed: " .. (err or "unknown"), vim.log.levels.ERROR, { title = "sakuin" })
		end
		local waiters = build_pending
		build_pending = {}
		on_ready(ok)
		for _, cb in ipairs(waiters) do
			cb(ok)
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

	try_download_async(opts.version, vim.schedule_wrap(function(dl_ok, dl_err)
		download_in_progress = false
		local waiters = download_pending
		download_pending = {}

		if dl_ok then
			vim.notify("Prebuilt binary installed.", vim.log.levels.INFO, { title = "sakuin" })
			on_ready(true)
			for _, cb in ipairs(waiters) do
				cb(true)
			end
			return
		end

		vim.notify("Prebuilt download failed: " .. (dl_err or "unknown"), vim.log.levels.WARN, { title = "sakuin" })

		-- Give snacks one scheduler tick to render the WARN before the next notify.
		vim.schedule(function()
			start_or_join_build(on_ready)
			for _, cb in ipairs(waiters) do
				start_or_join_build(cb)
			end
		end)
	end))
end

return M
