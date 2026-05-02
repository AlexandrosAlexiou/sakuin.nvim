local sakuin_ffi = require("sakuin.ffi")
local sakuin_config = require("sakuin.config")

local M = {}

---@param opts? snacks.picker.Config
function M.sakuin(opts)
	opts = opts or {}
	local config = sakuin_config.get()
	local search_config = config.search or {}
	local search_limit = search_config.limit or 10000

	-- Bumped on every new search; ffi dispatcher uses it to drop stale batches.
	local generation = 0

	---@param ctx snacks.picker.finder.ctx
	---@return snacks.picker.finder.async
	---@diagnostic disable-next-line: unused-local
	local function finder(_fopts, ctx)
		---@async
		return function(callback)
			local search = ctx.filter and ctx.filter.search or ""
			if search == "" then
				return
			end

			generation = generation + 1
			local my_gen = generation
			local done = false

			-- ffi dispatcher auto-unregisters this callback on done/error,
			-- so the coroutine always receives a terminal event and unwinds —
			-- even when a newer query supersedes us.
			local pending_items = {} ---@type snacks.picker.finder.Item[]
			local error_msg = nil ---@type string?

			sakuin_ffi.register_search_callback(my_gen, function(msg_type, results, err)
				if msg_type == "batch" and results then
					for _, r in ipairs(results) do
						local col_nr = r.col - 1 -- 0-indexed for snacks
						pending_items[#pending_items + 1] = {
							file = r.path,
							text = r.path .. ":" .. r.line .. ":" .. col_nr .. ":" .. r.snippet,
							pos = { r.line, col_nr },
							line = r.snippet,
							score_offset = r.score,
						}
					end
					ctx.async:resume()
				elseif msg_type == "done" then
					done = true
					ctx.async:resume()
				elseif msg_type == "error" then
					error_msg = err or "unknown"
					done = true
					ctx.async:resume()
				end
			end)

			local rc, submit_err = sakuin_ffi.search_submit(search, my_gen, search_limit)
			if rc ~= 0 then
				sakuin_ffi.unregister_search_callback(my_gen)
				if submit_err then
					vim.schedule(function()
						vim.notify("[sakuin] " .. submit_err, vim.log.levels.ERROR)
					end)
				end
				return
			end

			-- Not a busy loop: ctx.async:suspend() yields the coroutine entirely.
			-- The search callback calls ctx.async:resume() once per batch/done/error,
			-- so this loop body executes exactly once per rust event, no polling.
			-- The `if not done` guard prevents a final suspend after the done event,
			-- which would leave the coroutine suspended and never collected.
			while not done do
				if #pending_items > 0 then
					local items = pending_items
					pending_items = {}
					for _, item in ipairs(items) do
						callback(item)
					end
				end

				if not done then
					ctx.async:suspend()
				end
			end

			for _, item in ipairs(pending_items) do
				callback(item)
			end

			-- Belt-and-braces: dispatcher already removed us on done/error,
			-- but ensure we're gone if this path was hit via some other exit.
			sakuin_ffi.unregister_search_callback(my_gen)

			if error_msg then
				vim.schedule(function()
					vim.notify("[sakuin] search error: " .. error_msg, vim.log.levels.ERROR)
				end)
			end
		end
	end

	local picker_opts = vim.tbl_deep_extend("force", {
		title = "Sakuin Search",
		finder = finder,
		format = "file",
		preview = "file",
		live = true,
		supports_live = true,
		-- Disable snacks' built-in fuzzy matcher: Tantivy handles ranking.
		matcher = { fuzzy = false, sort_empty = false, smartcase = false, ignorecase = false },
		sort = { fields = { "idx" } },
		show_empty = true,
		on_close = function()
			sakuin_ffi.search_cancel()
			sakuin_ffi.clear_search_callbacks()
		end,
	}, opts)

	Snacks.picker.pick(picker_opts)
end

return M
