--- sakuin.nvim — Snacks picker for indexed full-text search.
--- Search runs on a persistent background thread; results stream in as batches.
--- Uses snacks.nvim's live finder with async item delivery for incremental display.
--- Display uses snacks' built-in `format = "file"` for consistent look with grep.

local sakuin_ffi = require("sakuin.ffi")
local sakuin_config = require("sakuin.config")

local M = {}

-- ---------------------------------------------------------------------------
-- Snacks picker source
-- ---------------------------------------------------------------------------

--- Open the sakuin search picker.
---@param opts? snacks.picker.Config
function M.sakuin(opts)
	opts = opts or {}
	local config = sakuin_config.get()
	local search_config = config.search or {}
	local batch_size = search_config.batch_size or 500
	local search_limit = search_config.limit or 10000

	-- Generation counter: incremented on every new search to discard stale results
	local generation = 0

	--- The finder function for snacks.
	--- Returns an async function that submits the search to Rust and yields items
	--- as streaming batches arrive via uv_async_send.
	---@param ctx snacks.picker.finder.ctx
	---@return snacks.picker.finder.async
	---@diagnostic disable-next-line: unused-local
	local function finder(_fopts, ctx)
		---@async
		return function(cb)
			local search = ctx.filter and ctx.filter.search or ""
			if search == "" then
				return
			end

			generation = generation + 1
			local my_gen = generation
			local done = false

			-- Register the streaming callback.
			-- This runs on the main Neovim thread (via vim.schedule in ffi.lua).
			-- We accumulate items into a shared table and wake the async coroutine.
			local pending_items = {} ---@type snacks.picker.finder.Item[]
			local error_msg = nil ---@type string?

			sakuin_ffi.set_search_callback(function(msg_type, msg_generation, results, err)
				if msg_generation ~= my_gen then
					return
				end

				if msg_type == "batch" and results then
					for _, r in ipairs(results) do
						local line_nr = r.line or 0
						local col_nr = (r.col or 1) - 1 -- 0-indexed for snacks
						local snippet = (r.snippet or ""):match("^%s*(.-)%s*$") or ""
						pending_items[#pending_items + 1] = {
							file = r.path,
							-- text is used for fuzzy matching (same format as grep source)
							text = r.path .. ":" .. line_nr .. ":" .. col_nr .. ":" .. snippet,
							pos = { line_nr, col_nr },
							-- line is the snippet displayed after the filename by format="file"
							line = snippet,
							score_offset = r.score or 0,
						}
					end
					-- Wake the async coroutine if it's sleeping
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

			-- Submit the search to the Rust worker
			local rc, submit_err = sakuin_ffi.search_submit(search, my_gen, batch_size, search_limit)
			if rc ~= 0 then
				if submit_err then
					vim.schedule(function()
						vim.notify("[sakuin] " .. submit_err, vim.log.levels.ERROR)
					end)
				end
				return
			end

			-- Async loop: yield items as they arrive from the Rust worker.
			-- We sleep and get woken up when new batches arrive.
			while not done do
				-- Yield any pending items to snacks
				while #pending_items > 0 do
					local items = pending_items
					pending_items = {}
					for _, item in ipairs(items) do
						cb(item)
					end
				end

				if not done then
					ctx.async:suspend()
				end
			end

			-- Yield any remaining items after done
			for _, item in ipairs(pending_items) do
				cb(item)
			end

			-- Clear the callback
			sakuin_ffi.set_search_callback(nil)

			if error_msg then
				vim.schedule(function()
					vim.notify("[sakuin] search error: " .. error_msg, vim.log.levels.ERROR)
				end)
			end
		end
	end

	-- Merge user opts with our source config
	local picker_opts = vim.tbl_deep_extend("force", {
		title = "Sakuin Search",
		finder = finder,
		-- Use snacks' built-in file formatter: icon, file:line:col, treesitter-highlighted snippet
		format = "file",
		preview = "file",
		live = true,
		supports_live = true,
		-- Disable snacks' built-in fuzzy matcher: Tantivy handles ranking.
		matcher = { fuzzy = false, sort_empty = false, smartcase = false, ignorecase = false },
		sort = { fields = { "idx" } },
		-- Show even when no results yet (user is typing a query)
		show_empty = true,
		on_close = function()
			sakuin_ffi.search_cancel()
			sakuin_ffi.set_search_callback(nil)
		end,
	}, opts)

	Snacks.picker.pick(picker_opts)
end

return M
