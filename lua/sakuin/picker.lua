local sakuin_ffi = require("sakuin.ffi")
local sakuin_config = require("sakuin.config")

local M = {}

---@param opts? snacks.picker.Config
function M.sakuin(opts)
	opts = opts or {}
	local search_limit = sakuin_config.get().search.limit
	local search_debounce = sakuin_config.get().search.debounce

	local generation = 0

	local function emit_items(results, callback)
		for _, r in ipairs(results) do
			local snippet_pos = #r.positions > 0 and r.positions or nil
			-- col sits on the first highlighted byte, so this is the whitespace
			-- trimmed off the snippet. Always set: skips snacks' per-frame regex.
			local file_pos = {}
			if snippet_pos then
				local trim = r.col - snippet_pos[1]
				for i, p in ipairs(snippet_pos) do
					file_pos[i] = p + trim
				end
			end

			callback({
				file = r.path,
				pos = { r.line, r.col - 1 }, -- snacks columns are 0-based
				line = r.snippet,
				score_offset = r.score,
				positions = file_pos,
				_match_pos = snippet_pos,
			})
		end
	end

	-- snacks throttles input to a finder:run every 200ms while typing and
	-- aborts the previous run each time. Sleeping here turns that throttle
	-- into a trailing debounce: a run superseded mid-sleep is aborted before
	-- it reaches search_submit, so only a typing pause submits a search.
	local function wait_for_typing_to_settle(ctx)
		if search_debounce > 0 then ctx.async:sleep(search_debounce) end
	end

	local function cancel_search_when_superseded(ctx, my_gen)
		ctx.async:on("abort", function()
			sakuin_ffi.unregister_search_callback(my_gen)
			if my_gen == generation then sakuin_ffi.search_cancel() end
		end)
	end

	---@param ctx snacks.picker.finder.ctx
	---@return snacks.picker.finder.async
	---@diagnostic disable-next-line: unused-local
	local function finder(_fopts, ctx)
		---@async
		return function(callback)
			local search = ctx.filter and ctx.filter.search or ""
			if search == "" then return end

			wait_for_typing_to_settle(ctx)

			generation = generation + 1
			local my_gen = generation
			local done = false
			local error_msg = nil ---@type string?

			local pending = {} ---@type table[]
			local yield = require("snacks.picker.util.async").yielder()

			sakuin_ffi.register_search_callback(my_gen, function(msg_type, results, err)
				if msg_type == "batch" and results then
					for _, r in ipairs(results) do
						pending[#pending + 1] = r
					end
				elseif msg_type == "done" then
					done = true
				elseif msg_type == "error" then
					error_msg = err or "unknown"
					done = true
				end
				ctx.async:resume()
			end)

			cancel_search_when_superseded(ctx, my_gen)

			local ok, submit_err = sakuin_ffi.search_submit(search, my_gen, search_limit)
			if not ok then
				sakuin_ffi.unregister_search_callback(my_gen)
				if submit_err then vim.schedule(function() vim.notify("[sakuin] " .. submit_err, vim.log.levels.ERROR) end) end
				return
			end

			while not done do
				if #pending > 0 then
					local batch = pending
					pending = {}
					emit_items(batch, callback)
					yield()
				end
				if not done then ctx.async:suspend() end
			end

			if #pending > 0 then emit_items(pending, callback) end

			sakuin_ffi.unregister_search_callback(my_gen)

			if error_msg then
				vim.schedule(function() vim.notify("[sakuin] search error: " .. error_msg, vim.log.levels.ERROR) end)
			end
		end
	end

	local function format_row(item, picker)
		local ret = Snacks.picker.format.filename(item, picker)
		if item.line then
			ret[#ret + 1] = { " " }
			if item._match_pos then
				local offset = Snacks.picker.highlight.offset(ret)
				Snacks.picker.highlight.matches(ret, item._match_pos, offset)
			end
			Snacks.picker.highlight.format(item, item.line, ret)
		end
		return ret
	end

	local picker_opts = vim.tbl_deep_extend("force", {
		title = "Sakuin Search",
		finder = finder,
		format = format_row,
		preview = "file",
		live = true,
		supports_live = true,
		-- Tantivy ranks results; disable snacks' fuzzy matcher.
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
