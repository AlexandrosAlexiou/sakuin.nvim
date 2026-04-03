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
  int         sakuin_search_submit(const char* query, uint64_t generation, uint64_t batch_size, uint64_t limit);
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

  /* libuv — we only need the send function pointer */
  int         uv_async_send(void* handle);
]])

local M = {}

---@type ffi.namespace*|nil
local lib = nil

--- Return the loaded native library, or error if not yet loaded.
---@return ffi.namespace*
local function get_lib()
	if not lib then
		error("[sakuin] native library not loaded — call require('sakuin').setup() first", 2)
	end
	return lib
end

--- The registered Lua callback for search results (streaming).
--- Set via M.set_search_callback(). Always called on the main Neovim thread.
--- Signature: callback(msg_type, generation, results_or_nil, error_or_nil, total_so_far_or_nil)
--- msg_type is "batch", "done", or "error".
---@type fun(msg_type: string, generation: number, results: table|nil, error: string|nil, total: number|nil)|nil
local lua_search_callback = nil

--- The registered Lua callback for indexing completion events.
--- Set via M.set_indexing_callback(). Always called on the main Neovim thread.
---@type fun(event: table)|nil   event = { status, total, done, error? }
local lua_indexing_callback = nil

--- The uv_async handle. Kept alive to prevent GC.
local async_handle = nil

--- Resolve the path to the native library based on the current OS.
---@return string path Absolute path to the shared library
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

--- Called on the main Neovim thread when the Rust worker signals via uv_async_send.
--- The same async channel carries both search results and indexing events; we drain
--- both slots every time we wake up.
local function on_async_notification()
	if not lib then
		return
	end

	-- Check for an indexing completion event (pushed by build/update async threads).
	local idx_raw = lib.sakuin_indexing_take_event()
	if idx_raw ~= nil then
		local json_str = ffi.string(idx_raw)
		lib.sakuin_free_string(idx_raw)
		local ok, event = pcall(vim.json.decode, json_str)
		if ok and lua_indexing_callback then
			lua_indexing_callback(event)
		end
	end

	-- Drain all search result messages from the queue.
	-- The worker pushes batch/done/error messages; we may have multiple per wake-up.
	while true do
		local raw = lib.sakuin_search_take_result()
		if raw == nil then
			break -- queue is empty
		end

		local json_str = ffi.string(raw)
		lib.sakuin_free_string(raw)

		local ok, msg = pcall(vim.json.decode, json_str)
		if not ok then
			if lua_search_callback then
				lua_search_callback("error", 0, nil, "Failed to decode search message: " .. tostring(msg))
			end
		elseif lua_search_callback then
			local gen = msg.generation or 0
			local msg_type = msg.type or "error"

			if msg_type == "batch" then
				lua_search_callback("batch", gen, msg.results, nil, msg.total_so_far)
			elseif msg_type == "done" then
				lua_search_callback("done", gen, nil, nil, msg.total)
			elseif msg_type == "error" then
				lua_search_callback("error", gen, nil, msg.error)
			end
		end
	end
end

--- Load the native library. Must be called before any other FFI function.
function M.load()
	if lib then
		return
	end

	local lib_path = resolve_lib_path()
	if vim.fn.filereadable(lib_path) == 0 then
		error(
			"[sakuin] Native library not found at: "
				.. lib_path
				.. "\nRun :SakuinBuild or install via your plugin manager's build hook."
		)
	end

	lib = ffi.load(lib_path)

	-- Create a libuv async handle. Its callback runs on the main Neovim thread.
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

	-- Get the uv_async_send function pointer.
	-- Neovim links libuv and exports its symbols in the process's global symbol
	-- table. ffi.C accesses these symbols on all platforms.
	-- Signature: int uv_async_send(uv_async_t* handle)
	-- Docs: https://docs.libuv.org/en/v1.x/async.html
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

	-- Register with Rust so the worker thread can call uv_async_send(handle)
	lib.sakuin_register_async_notifier(ffi.cast("void*", handle_ptr), ffi.cast("void*", send_fn_ptr))
end

--- Check if the native library is loaded.
---@return boolean
function M.is_loaded()
	return lib ~= nil
end

--- Initialize the engine.
---@param project_root string Absolute path to the project directory
---@param index_dir string Absolute path to the index directory
---@param config_json string JSON-serialized SakuinConfig (ignored if nil)
---@return number rc 0 on success, -1 on error
function M.init(project_root, index_dir, config_json)
	return get_lib().sakuin_init(project_root, index_dir, config_json)
end

--- Shut down the engine.
function M.shutdown()
	if lib then
		lib.sakuin_shutdown()
	end
	-- Close the async handle
	if async_handle and not async_handle:is_closing() then
		async_handle:close()
	end
end

--- Full index rebuild.
---@return number rc 0 on success, -1 on error
function M.build_index()
	return get_lib().sakuin_build_index()
end

--- Incremental index update.
---@return number rc 0 on success, -1 on error
function M.update_index()
	return get_lib().sakuin_update_index()
end

--- Start the filesystem watcher.
---@return number rc 0 on success, -1 on error
function M.start_watcher()
	return get_lib().sakuin_start_watcher()
end

--- Stop the filesystem watcher.
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

--- Set the Lua callback for async search results (streaming).
---
--- The callback receives: (msg_type, generation, results_or_nil, error_or_nil, total_or_nil)
---   - msg_type "batch": results is an array of matches, total is cumulative count so far
---   - msg_type "done": results is nil, total is final count
---   - msg_type "error": error is the error message
--- It is always called on the main Neovim thread (via uv_async + vim.schedule).
---@param callback fun(msg_type: string, generation: number, results: table|nil, error: string|nil)|nil
function M.set_search_callback(callback)
	lua_search_callback = callback
end

--- Submit a search query to the persistent worker thread (non-blocking).
---
--- Results are delivered as streaming batches via the registered callback.
--- Any in-flight search is automatically cancelled.
---@param query string The search query
---@param generation number Monotonically increasing generation counter
---@param batch_size? number Results per batch (0 or nil = default 500)
---@param limit? number Maximum total results (0 or nil = unlimited)
---@return number rc 0 on success, -1 on error
---@return string|nil error Error message on failure
function M.search_submit(query, generation, batch_size, limit)
	local rc = get_lib().sakuin_search_submit(query, generation, batch_size or 0, limit or 0)
	if tonumber(rc) ~= 0 then
		return -1, M.last_error() or "search_submit failed"
	end
	return 0, nil
end

--- Cancel any in-flight async search.
function M.search_cancel()
	get_lib().sakuin_search_cancel()
end

--- Execute a search query (synchronous — blocks the caller).
---@param query string The search query
---@return table|nil results Array of {path, line, col, snippet, score}
---@return string|nil error Error message on failure
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

--- Get index statistics.
---@return table|nil stats {num_docs, num_segments, index_size_bytes, project_root}
---@return string|nil error Error message on failure
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

--- Spawn a full index rebuild on a background thread (non-blocking).
---@return number rc 0 on success, -1 on error
function M.build_index_async()
	return get_lib().sakuin_build_index_async()
end

--- Spawn an incremental index update on a background thread (non-blocking).
---@return number rc 0 on success, -1 on error
function M.update_index_async()
	return get_lib().sakuin_update_index_async()
end

--- Get the progress of the current async build/update operation.
---@return table|nil progress {total, done, status} where status is "idle"|"running"|"done"|"error"
---@return string|nil error Error message on failure
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

--- Get the last error message from the Rust side.
---@return string|nil error The error message, or nil if no error
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

return M
