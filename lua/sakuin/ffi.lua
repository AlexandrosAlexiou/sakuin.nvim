--- sakuin.nvim — LuaJIT FFI bindings to the native Rust library.
---
--- Async search architecture:
---   1. A `vim.uv.new_async(callback)` handle runs on the main Neovim thread.
---   2. The raw `uv_async_t*` + `uv_async_send` address are passed to Rust.
---   3. Rust pushes result batches + terminal messages to a VecDeque, calling
---      `uv_async_send` for each.
---   4. The async callback drains the queue, checks generation, and dispatches.

local ffi = require("ffi")

ffi.cdef([[
  /* Lifecycle */
  int         sakuin_init(const char* project_root, const char* index_dir, const char* config_json);
  void        sakuin_shutdown(void);

  /* Indexing */
  int         sakuin_build_index_async(void);
  int         sakuin_update_index_async(void);

  /* Watcher */
  int         sakuin_start_watcher(void);
  void        sakuin_stop_watcher(void);

  /* Search — async with result queue + uv_async notification */
  void        sakuin_register_async_notifier(void* handle_ptr, void* send_fn_ptr);
  const char* sakuin_search_take_result(void);
  int         sakuin_search_submit(const char* query, uint64_t generation, uint64_t limit);
  void        sakuin_search_cancel(void);

  /* Indexing completion event (pushed via uv_async_send, same channel as search) */
  const char* sakuin_indexing_take_event(void);

  /* Info */
  const char* sakuin_stats(void);
  const char* sakuin_last_error(void);

  /* Memory */
  void        sakuin_free_string(const char* s);

  /* Logging */
  int         sakuin_set_log_level(const char* level);
  void        sakuin_clear_logs(void);

  /* libuv — we only need the send function pointer */
  int         uv_async_send(void* handle);
]])

local M = {}

---@type ffi.namespace*|nil
local lib = nil

---@return ffi.namespace*
local function get_lib()
	if not lib then
		error("[sakuin] native library not loaded — call require('sakuin').setup() first", 2)
	end
	return lib
end

-- Per-generation search callbacks. Dispatcher routes messages by generation and
-- unregisters on terminal events so stale coroutines unwind and are collected.
---@type table<number, fun(msg_type: string, results: table|nil, error: string|nil, total: number|nil)>
local search_callbacks = {}

---@type fun(event: table)|nil
local lua_indexing_callback = nil

local async_handle = nil -- kept alive to prevent GC

---@return string
local function resolve_lib_path()
	local source = debug.getinfo(1, "S").source:sub(2) -- strip leading @
	local plugin_root = vim.fn.fnamemodify(source, ":h:h:h")

	local os_name = jit.os -- "Windows", "Linux", "OSX"
	local exts = { Windows = ".dll", OSX = ".dylib" }
	local ext = exts[os_name] or ".so"

	local prefix = os_name == "Windows" and "" or "lib"
	return plugin_root .. "/build/" .. prefix .. "sakuin" .. ext
end

local function on_async_notification()
	if not lib then
		return
	end

	local idx_raw = lib.sakuin_indexing_take_event()
	if idx_raw ~= nil then
		local json_str = ffi.string(idx_raw)
		lib.sakuin_free_string(idx_raw)
		local ok, event = pcall(vim.json.decode, json_str)
		if ok and lua_indexing_callback then
			lua_indexing_callback(event)
		end
	end

	while true do
		local raw = lib.sakuin_search_take_result()
		if raw == nil then
			break
		end

		local json_str = ffi.string(raw)
		lib.sakuin_free_string(raw)

		local ok, msg = pcall(vim.json.decode, json_str)
		local callback = ok and search_callbacks[msg.generation]
		if callback then
			local msg_type = msg.type
			if msg_type == "batch" then
				callback("batch", msg.results, nil, msg.total_so_far)
			elseif msg_type == "done" then
				search_callbacks[msg.generation] = nil
				callback("done", nil, nil, msg.total)
			elseif msg_type == "error" then
				search_callbacks[msg.generation] = nil
				callback("error", nil, msg.error, nil)
			end
		end
	end
end

function M.load()
	if lib then
		return
	end

	local lib_path = resolve_lib_path()
	if vim.fn.filereadable(lib_path) == 0 then
		error(
			"[sakuin] Native library not found at: "
				.. lib_path
				.. "\nRun the build script (scripts/build.sh) or install a prebuilt binary from GitHub Releases."
		)
	end

	lib = ffi.load(lib_path)

	local schedule_pending = false
	async_handle = vim.uv.new_async(function()
		if not schedule_pending then
			schedule_pending = true
			vim.schedule(function()
				schedule_pending = false
				on_async_notification()
			end)
		end
	end)

	-- luv userdata is a void** to the raw uv_async_t*; dereference once.
	local handle_ptr = ffi.cast("void**", async_handle)[0]

	-- Resolve uv_async_send from Neovim's exported libuv symbols.
	local ok_send, send_fn_ptr = pcall(function()
		return ffi.C.uv_async_send
	end)
	if not ok_send then
		error(
			"[sakuin] Could not resolve uv_async_send from the C namespace. "
				.. "This is required for async search. Error: "
				.. tostring(send_fn_ptr)
		)
	end

	lib.sakuin_register_async_notifier(ffi.cast("void*", handle_ptr), ffi.cast("void*", send_fn_ptr))
end

---@return boolean
function M.is_loaded()
	return lib ~= nil
end

---@param project_root string
---@param index_dir string
---@param config_json string
---@return number
function M.init(project_root, index_dir, config_json)
	return get_lib().sakuin_init(project_root, index_dir, config_json)
end

function M.shutdown()
	if lib then
		lib.sakuin_shutdown()
	end
	if async_handle and not async_handle:is_closing() then
		async_handle:close()
	end
end

---@return number
function M.start_watcher()
	return get_lib().sakuin_start_watcher()
end

function M.stop_watcher()
	if lib then
		lib.sakuin_stop_watcher()
	end
end

-- Callbacks fire on the main Neovim thread (via uv_async + vim.schedule).
-- Indexing event shape: { status="done"|"error", total, done, error? }.
---@param callback fun(event: table)|nil
function M.set_indexing_callback(callback)
	lua_indexing_callback = callback
end

-- Search callback args: (msg_type, results_or_nil, error_or_nil, total_or_nil).
-- "batch": results = matches, total = cumulative count so far.
-- "done":  results = nil,     total = final count.
-- "error": error  = message.
-- Dispatcher auto-unregisters on done/error; callers may unregister early
-- (e.g. picker close before any terminal event arrives).
---@param generation number
---@param callback fun(msg_type: string, results: table|nil, error: string|nil, total: number|nil)
function M.register_search_callback(generation, callback)
	search_callbacks[generation] = callback
end

---@param generation number
function M.unregister_search_callback(generation)
	search_callbacks[generation] = nil
end

function M.clear_search_callbacks()
	search_callbacks = {}
end

-- Submit a search to the persistent worker thread. Any in-flight search is
-- automatically cancelled. Results stream back via the registered callback.
---@param query string
---@param generation number
---@param limit? number 0 or nil = unlimited
---@return number rc 0 on success, -1 on error
---@return string|nil error
function M.search_submit(query, generation, limit)
	local rc = get_lib().sakuin_search_submit(query, generation, limit or 0)
	if tonumber(rc) ~= 0 then
		return -1, M.last_error() or "search_submit failed"
	end
	return 0, nil
end

function M.search_cancel()
	get_lib().sakuin_search_cancel()
end

---@return table|nil stats
---@return string|nil error
function M.stats()
	local l = get_lib()
	local raw = l.sakuin_stats()
	if raw == nil then
		return nil, M.last_error() or "stats returned null"
	end

	local json_str = ffi.string(raw)
	l.sakuin_free_string(raw)

	local ok, decoded = pcall(vim.json.decode, json_str)
	if not ok then
		return nil, "Failed to decode stats: " .. tostring(decoded)
	end

	return decoded, nil
end

---@return number
function M.build_index_async()
	return get_lib().sakuin_build_index_async()
end

---@return number
function M.update_index_async()
	return get_lib().sakuin_update_index_async()
end

---@return string|nil
function M.last_error()
	local l = get_lib()
	local raw = l.sakuin_last_error()
	if raw == nil then
		return nil
	end

	local msg = ffi.string(raw)
	l.sakuin_free_string(raw)
	return msg
end

---@param level "error"|"warn"|"info"|"debug"|"trace"|"off"
---@return boolean
function M.set_log_level(level)
	local rc = get_lib().sakuin_set_log_level(level)
	return tonumber(rc) == 0
end

function M.clear_logs()
	get_lib().sakuin_clear_logs()
end

return M
