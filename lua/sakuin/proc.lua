--- Drive a command-yielding coroutine. A flow is written once as `impl(run, ...)`;
--- `run(cmd, opts)` spawns a process and returns its result. One driver blocks on
--- each process (lazy's `build`), the other runs it async (runtime).

local M = {}

local function make_run()
	return function(cmd, opts) return coroutine.yield(cmd, opts) end
end

--- Blocking driver.
---@param impl fun(run: function, ...): boolean, string?
---@return boolean ok
---@return string? err
function M.drive_sync(impl, ...)
	local co = coroutine.create(impl)
	local ok, a, b = coroutine.resume(co, make_run(), ...)
	while ok and coroutine.status(co) ~= "dead" do
		ok, a, b = coroutine.resume(co, vim.system(a, b or { text = true }):wait())
	end
	if not ok then return false, a end
	return a, b
end

--- Non-blocking driver.
---@param impl fun(run: function, ...): boolean, string?
---@param on_done fun(ok: boolean, err?: string)
function M.drive_async(impl, on_done, ...)
	local co = coroutine.create(impl)
	local function step(...)
		local ok, a, b = coroutine.resume(co, ...)
		if not ok then return on_done(false, a) end
		if coroutine.status(co) == "dead" then return on_done(a, b) end
		vim.system(a, b or { text = true }, vim.schedule_wrap(function(result) step(result) end))
	end
	step(make_run(), ...)
end

return M
