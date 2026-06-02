local sakuin_ffi = require("sakuin.ffi")
local sakuin_config = require("sakuin.config")

local M = {}

---@param opts? snacks.picker.Config
function M.sakuin(opts)
	opts = opts or {}
	local config = sakuin_config.get()
	local search_config = config.search or {}
	local search_limit = search_config.limit or 10000
	local search_debounce = search_config.debounce or 150

	local generation = 0
	local query = ""

	local function compute_match_positions(text, q)
		if q == "" or not text or text == "" then return nil end
		local hay = text:lower()
		local positions = {}
		for term in q:gmatch("%S+") do
			local needle = term:lower()
			local from = 1
			while #needle > 0 do
				local s, e = hay:find(needle, from, true)
				if not s then break end
				for i = s, e do
					positions[#positions + 1] = i
				end
				from = e + 1
			end
		end
		if #positions == 0 then return nil end
		return positions
	end

	local function emit_items(results, callback, search)
		for _, r in ipairs(results) do
			local col = r.col - 1 -- snacks columns are 0-based
			local snippet_pos = compute_match_positions(r.snippet, search)

			-- Adjust positions for the untrimmed file line (preview panel).
			-- Must always be set to skip Snacks' per-frame regex matching.
			local file_pos = {}
			if snippet_pos then
				local hay = r.snippet:lower()
				local pos_in_snippet = hay:find(search:lower(), 1, true)
				if pos_in_snippet then
					local trim_offset = r.col - pos_in_snippet
					if trim_offset > 0 then
						for _, p in ipairs(snippet_pos) do
							file_pos[#file_pos + 1] = p + trim_offset
						end
					else
						file_pos = snippet_pos
					end
				end
			end

			callback({
				file = r.path,
				text = r.path .. ":" .. r.line .. ":" .. col .. ":" .. r.snippet,
				pos = { r.line, col },
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
			query = search
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
					emit_items(batch, callback, search)
					yield()
				end
				if not done then ctx.async:suspend() end
			end

			if #pending > 0 then emit_items(pending, callback, search) end

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
