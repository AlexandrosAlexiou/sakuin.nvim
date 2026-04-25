--- sakuin.nvim — LuaJIT FFI bindings to the native Rust library.
---
--- Architecture for async search (streaming batches):
---   1. We create a `vim.uv.new_async(callback)` handle. Its callback runs on
---      the main Neovim thread — safe for any Lua/API calls.
---   2. We pass the raw `uv_async_t*` handle pointer and the address of
---      `uv_async_send` to Rust via `sakuin_register_async_notifier`.
---   3. The Rust worker thread pushes result batches + a terminal (done/error)
---      message to a VecDeque queue, calling `uv_async_send(handle)` for each.
---   4. Our async callback drains the queue via `sakuin_search_take_result()`,
---      decodes each JSON message, checks generation, and invokes the Lua callback
---      with message type ("batch", "done", or "error").
---
--- This streaming design avoids serializing/transferring all results in a single
--- JSON blob, keeping FFI payloads small and delivering results to the picker
--- incrementally as they are found (like ripgrep + snacks.nvim).

local ffi = require("ffi")

ffi.cdef([[
  /* Lifecycle */
  int         sakuin_init(const char* project_root, const char* index_dir, const char* config_json);
  void        sakuin_shutdown(void);

  /* Indexing */
  int         sakuin_build_index(void);
  int         sakuin_update_index(void);
  int         sakuin_build_index_async(void);
  int         sakuin_update_index_async(void);
  const char* sakuin_get_progress(void);

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

  /* Search — synchronous (blocks caller) */
  const char* sakuin_search(const char* query);

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

--- Per-generation search callbacks. Each in-flight finder coroutine
--- registers itself under its own generation; the dispatcher routes
--- messages by generation and unregisters on terminal events so stale
--- coroutines unwind and are collected.
---@type table<number, fun(msg_type: string, results: table|nil, error: string|nil, total: number|nil)>
local search_callbacks = {}

---@type fun(event: table)|nil
local lua_indexing_callback = nil

local async_handle = nil -- kept alive to prevent GC

---@return string
local function resolve_lib_path()
	-- Get the plugin root directory from this file's location:
	-- lua/sakuin/ffi.lua -> ../../ -> plugin root
	local source = debug.getinfo(1, "S").source:sub(2) -- strip leading @
	local plugin_root = vim.fn.fnamemodify(source, ":h:h:h")

	local os_name = jit.os -- "Windows", "Linux", "OSX"
	local ext
	if os_name == "Windows" then
		ext = ".dll"
	elseif os_name == "OSX" then
		ext = ".dylib"
	else
		ext = ".so"
	end

	local prefix = os_name == "Windows" and "" or "lib"
	return plugin_root .. "/build/" .. prefix .. "sakuin" .. ext
end

-- Called on the main Neovim thread via uv_async_send. Drains both the indexing
-- event slot and the search result queue on every wake-up.
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
		local cb = ok and search_callbacks[msg.generation]
		if cb then
			local msg_type = msg.type
			if msg_type == "batch" then
				cb("batch", msg.results, nil, msg.total_so_far)
			elseif msg_type == "done" then
				search_callbacks[msg.generation] = nil
				cb("done", nil, nil, msg.total)
			elseif msg_type == "error" then
				search_callbacks[msg.generation] = nil
				cb("error", nil, msg.error, nil)
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

	async_handle = vim.uv.new_async(function()
		-- vim.schedule to ensure we're fully in the Neovim event loop context
		vim.schedule(on_async_notification)
	end)

	-- Get the raw uv_async_t* pointer from the luv userdata.
	--
	-- luv (vim.uv) allocates libuv handles on the heap via malloc() and stores
	-- a pointer to them in the Lua userdata payload. The userdata is effectively
	-- a void** where [0] is the raw uv_async_t*. We cast to void** and
	-- dereference once to get the actual handle pointer.
	local handle_ptr = ffi.cast("void**", async_handle)[0]

	-- Neovim exports libuv symbols into the process global symbol table;
	-- ffi.C accesses them on all platforms. Docs: https://docs.libuv.org/en/v1.x/async.html
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
function M.build_index()
	return get_lib().sakuin_build_index()
end

---@return number
function M.update_index()
	return get_lib().sakuin_update_index()
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

--- Set the Lua callback for indexing completion events.
---
--- The callback receives a table: { status="done"|"error", total, done, error? }
--- It is always called on the main Neovim thread (via uv_async + vim.schedule).
---@param callback fun(event: table)|nil
function M.set_indexing_callback(callback)
	lua_indexing_callback = callback
end

--- Register a per-generation Lua callback for streaming async search results.
---
--- The callback receives: (msg_type, results_or_nil, error_or_nil, total_or_nil)
---   - msg_type "batch": results is an array of matches, total is cumulative count so far
---   - msg_type "done": results is nil, total is final count
---   - msg_type "error": error is the error message
---
--- The dispatcher auto-unregisters on `done` or `error`; callers may also
--- call `unregister_search_callback` explicitly for early teardown (e.g. on
--- picker close before any terminal event has arrived).
--- It is always called on the main Neovim thread (via uv_async + vim.schedule).
---@param generation number
---@param callback fun(msg_type: string, results: table|nil, error: string|nil, total: number|nil)
function M.register_search_callback(generation, callback)
	search_callbacks[generation] = callback
end

---@param generation number
function M.unregister_search_callback(generation)
	search_callbacks[generation] = nil
end

--- Drop all in-flight search callbacks. Defensive cleanup for picker close.
function M.clear_search_callbacks()
	search_callbacks = {}
end

--- Submit a search query to the persistent worker thread (non-blocking).
---
--- Results are delivered as streaming batches via the registered callback.
--- Any in-flight search is automatically cancelled.
---@param query string The search query
---@param generation number Monotonically increasing generation counter
---@param limit? number Maximum total results (0 or nil = unlimited)
---@return number rc 0 on success, -1 on error
---@return string|nil error Error message on failure
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

--- Synchronous search — blocks the caller.
---@param query string
---@return table|nil results
---@return string|nil error
function M.search(query)
	local l = get_lib()
	local raw = l.sakuin_search(query)
	if raw == nil then
		return nil, M.last_error() or "search returned null"
	end

	local json_str = ffi.string(raw)
	l.sakuin_free_string(raw)

	local ok, decoded = pcall(vim.json.decode, json_str)
	if not ok then
		return nil, "Failed to decode search results: " .. tostring(decoded)
	end

	return decoded, nil
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

---@return table|nil progress {total, done, status}
---@return string|nil error
function M.get_progress()
	local l = get_lib()
	local raw = l.sakuin_get_progress()
	if raw == nil then
		return nil, M.last_error() or "get_progress returned null"
	end

	local json_str = ffi.string(raw)
	l.sakuin_free_string(raw)

	local ok, decoded = pcall(vim.json.decode, json_str)
	if not ok then
		return nil, "Failed to decode progress: " .. tostring(decoded)
	end

	return decoded, nil
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

--- Change the log level at runtime.
---@param level string One of "error", "warn", "info", "debug", "trace", "off"
---@return boolean success
function M.set_log_level(level)
	local rc = get_lib().sakuin_set_log_level(level)
	return tonumber(rc) == 0
end

--- Clear the log file contents.
function M.clear_logs()
	get_lib().sakuin_clear_logs()
end

return M
