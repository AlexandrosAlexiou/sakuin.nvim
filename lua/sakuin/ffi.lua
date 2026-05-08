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
  typedef struct {
    const char* path;
    const char* snippet;
    uint32_t    line;
    uint32_t    col;
    float       score;
  } SakuinSearchResult;

  typedef struct {
    uint8_t     msg_type;
    uint64_t    generation;
    uint64_t    total;
    SakuinSearchResult* results;
    uint32_t    results_len;
    const char* error;
  } SakuinSearchMessage;

  typedef struct {
    uint8_t     status;
    uint64_t    total;
    uint64_t    done;
    const char* error;
    const char* message;
  } SakuinIndexingEvent;

  typedef struct {
    uint64_t    num_docs;
    uint32_t    num_segments;
    uint64_t    index_size_bytes;
    const char* project_root;
  } SakuinIndexStats;

  // Result of a fallible call. err is null on success, or a heap-allocated
  // string the caller must free with sakuin_free_string.
  typedef struct {
    const char* err;
  } SakuinStatus;

  SakuinStatus sakuin_init(const char* project_root, const char* index_dir, const char* config_json);
  void         sakuin_shutdown(void);

  SakuinStatus sakuin_build_index_async(void);
  SakuinStatus sakuin_update_index_async(void);

  SakuinStatus sakuin_start_watcher(void);
  void         sakuin_stop_watcher(void);

  void                    sakuin_register_async_notifier(void* handle_ptr, void* send_fn_ptr);
  SakuinSearchMessage*    sakuin_search_take_result(void);
  void                    sakuin_free_search_message(SakuinSearchMessage* ptr);
  SakuinStatus            sakuin_search_submit(const char* query, uint64_t generation, uint64_t limit);
  void                    sakuin_search_cancel(void);

  SakuinIndexingEvent*    sakuin_indexing_take_event(void);
  void                    sakuin_free_indexing_event(SakuinIndexingEvent* ptr);

  SakuinStatus            sakuin_stats(SakuinIndexStats** out);
  void                    sakuin_free_stats(SakuinIndexStats* ptr);

  void         sakuin_free_string(const char* s);

  SakuinStatus sakuin_set_log_level(const char* level);
  void         sakuin_clear_logs(void);

  int          uv_async_send(void* handle);
]])

local M = {}

---@type ffi.namespace*|nil
local lib = nil

--- Whether the Rust engine state is initialized (distinct from lib being loaded).
local initialized = false

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

-- ffi.string segfaults on NULL (calls strlen).
local function safe_str(p)
	if p == nil then
		return ""
	end
	return ffi.string(p)
end

-- Drain a SakuinStatus: returns nil on success, or the error message on
-- failure. Frees the heap-allocated error string on the way out.
local function status_to_err(status)
	if status.err == nil then
		return nil
	end
	local msg = ffi.string(status.err)
	lib.sakuin_free_string(status.err)
	return msg
end

--- Convert a CSearchMessage pointer to Lua-friendly data and free it.
local function decode_search_message(ptr)
	local msg_type = tonumber(ptr.msg_type)
	local generation = tonumber(ptr.generation)

	if msg_type == 0 then -- Batch
		local results = {}
		local len = tonumber(ptr.results_len)
		for i = 0, len - 1 do
			local r = ptr.results[i]
			results[#results + 1] = {
				path = safe_str(r.path),
				snippet = safe_str(r.snippet),
				line = tonumber(r.line),
				col = tonumber(r.col),
				score = tonumber(r.score),
			}
		end
		local total = tonumber(ptr.total)
		lib.sakuin_free_search_message(ptr)
		return "batch", generation, results, total
	elseif msg_type == 1 then -- Done
		local total = tonumber(ptr.total)
		lib.sakuin_free_search_message(ptr)
		return "done", generation, nil, total
	else -- Error
		local err = ptr.error ~= nil and ffi.string(ptr.error) or "unknown error"
		lib.sakuin_free_search_message(ptr)
		return "error", generation, err, nil
	end
end

--- Convert a CIndexingEvent pointer to a Lua table and free it.
local function decode_indexing_event(ptr)
	local status_code = tonumber(ptr.status)
	local status
	if status_code == 0 then
		status = "done"
	elseif status_code == 1 then
		status = "error"
	else
		status = "progress"
	end

	local event = {
		status = status,
		total = tonumber(ptr.total),
		done = tonumber(ptr.done),
	}
	if ptr.error ~= nil then
		event.error = ffi.string(ptr.error)
	end
	if ptr.message ~= nil then
		event.message = ffi.string(ptr.message)
	end

	lib.sakuin_free_indexing_event(ptr)
	return event
end

local function on_async_notification()
	if not lib then
		return
	end

	local idx_raw = lib.sakuin_indexing_take_event()
	if idx_raw ~= nil then
		local event = decode_indexing_event(idx_raw)
		if lua_indexing_callback then
			lua_indexing_callback(event)
		end
	end

	while true do
		local raw = lib.sakuin_search_take_result()
		if raw == nil then
			break
		end

		local msg_type, generation, data, total = decode_search_message(raw)
		local callback = search_callbacks[generation]
		if callback then
			if msg_type == "batch" then
				callback("batch", data, nil, total)
			elseif msg_type == "done" then
				search_callbacks[generation] = nil
				callback("done", nil, nil, total)
			elseif msg_type == "error" then
				search_callbacks[generation] = nil
				callback("error", nil, data, nil)
			end
		end
	end
end

local function setup_async_handle()
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

function M.load()
	if lib and async_handle then
		return
	end

	if not lib then
		local lib_path = resolve_lib_path()
		if vim.fn.filereadable(lib_path) == 0 then
			error(
				"[sakuin] Native library not found at: "
					.. lib_path
					.. "\nRun the build script (scripts/build.sh) or install a prebuilt binary from GitHub Releases."
			)
		end

		lib = ffi.load(lib_path)
	end

	if not async_handle then
		setup_async_handle()
	end
end

---@return boolean
function M.is_loaded()
	return lib ~= nil and initialized
end

---@param project_root string
---@param index_dir string
---@param config_json string
---@return boolean ok
---@return string|nil err
function M.init(project_root, index_dir, config_json)
	local err = status_to_err(get_lib().sakuin_init(project_root, index_dir, config_json))
	if err then
		return false, err
	end
	initialized = true
	return true, nil
end

function M.shutdown()
	initialized = false
	if lib then
		lib.sakuin_shutdown()
	end
	if async_handle and not async_handle:is_closing() then
		async_handle:close()
	end
	async_handle = nil
end

--- Reinitialize for a new project root. Shuts down the current state,
--- recreates the async handle, and calls sakuin_init with the new paths.
---@param project_root string
---@param index_dir string
---@param config_json string
---@return boolean ok
---@return string|nil err
function M.reinit(project_root, index_dir, config_json)
	-- Shut down the existing Rust state (watcher, writer, etc.)
	M.shutdown()

	-- Recreate the async handle for uv notifications
	setup_async_handle()

	local err = status_to_err(get_lib().sakuin_init(project_root, index_dir, config_json))
	if err then
		return false, err
	end
	initialized = true
	return true, nil
end

---@return boolean ok
---@return string|nil err
function M.start_watcher()
	local err = status_to_err(get_lib().sakuin_start_watcher())
	return err == nil, err
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
---@return boolean ok
---@return string|nil err
function M.search_submit(query, generation, limit)
	local err = status_to_err(get_lib().sakuin_search_submit(query, generation, limit or 0))
	return err == nil, err
end

function M.search_cancel()
	get_lib().sakuin_search_cancel()
end

---@return table|nil stats
---@return string|nil err
function M.stats()
	local l = get_lib()
	local out = ffi.new("SakuinIndexStats*[1]")
	local err = status_to_err(l.sakuin_stats(out))
	if err then
		return nil, err
	end

	local raw = out[0]
	if raw == nil then
		return nil, "stats returned null"
	end

	local stats = {
		num_docs = tonumber(raw.num_docs),
		num_segments = tonumber(raw.num_segments),
		index_size_bytes = tonumber(raw.index_size_bytes),
		project_root = ffi.string(raw.project_root),
	}
	l.sakuin_free_stats(raw)

	return stats, nil
end

---@return boolean ok
---@return string|nil err
function M.build_index_async()
	local err = status_to_err(get_lib().sakuin_build_index_async())
	return err == nil, err
end

---@return boolean ok
---@return string|nil err
function M.update_index_async()
	local err = status_to_err(get_lib().sakuin_update_index_async())
	return err == nil, err
end

---@param level "error"|"warn"|"info"|"debug"|"trace"|"off"
---@return boolean ok
---@return string|nil err
function M.set_log_level(level)
	local err = status_to_err(get_lib().sakuin_set_log_level(level))
	return err == nil, err
end

function M.clear_logs()
	get_lib().sakuin_clear_logs()
end

return M
