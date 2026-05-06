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

	--- Convert raw ffi results to picker items and pass them to callback.
	local function emit_items(results, callback)
		for _, r in ipairs(results) do
			local col = r.col - 1 -- 0-indexed for snacks
			callback({
				file = r.path,
				text = r.path .. ":" .. r.line .. ":" .. col .. ":" .. r.snippet,
				pos = { r.line, col },
				line = r.snippet,
				score_offset = r.score,
			})
		end
	end

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
			local error_msg = nil ---@type string?

			sakuin_ffi.register_search_callback(my_gen, function(msg_type, results, err)
				if msg_type == "batch" and results then
					emit_items(results, callback)
				elseif msg_type == "done" then
					done = true
				elseif msg_type == "error" then
					error_msg = err or "unknown"
					done = true
				end
				ctx.async:resume()
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

			while not done do
				ctx.async:suspend()
			end

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
