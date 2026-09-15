-- web: provider-native web search + URL fetch for the current model.
--
  -- Reference extension for model-aware Lua extensions: it registers
  -- provider-native tools (`search` everywhere, `fetch` on Gemini
  -- only — the model sees them as `lua__web__search` / `lua__web__fetch`;
  -- extension code uses the short names and the host resolves them),
  -- reuses the *current* model's endpoint + key (`dex.model`), hides tools
  -- the model cannot serve (`model_select` + `dex.tools.set_active`), and
  -- never switches the model silently (cost safety — an optional
  -- `override_model` in `dex.state` is the only override).
--
-- Copy this directory to `$XDG_CONFIG_HOME/dex/extensions/web`
-- (or `dex extensions install <dir>`) to use it.

return function(dex)
    local SEARCH_TOOL = "search"
    local FETCH_TOOL = "fetch"
    local OVERRIDE_KEY = "override_model"
    local MAX_TEXT = 8000
    -- Provider-side search/grounding rounds run ~40s+ non-streamed; keep
    -- this under the manifest per-tool `timeout` (120s).
    local NET_TIMEOUT_MS = 100000

    -- Which provider family serves this model. Gemini speaks its own search
    -- + url_context tools; OpenAI-Responses and Anthropic-Messages wires
    -- carry a web_search tool; anything else errors loudly at call time
    -- (never a silent model switch).
  local function provider_kind(provider, api)
    provider = string.lower(provider or "")
    api = api or ""
    if provider == "gemini" or provider == "google" then
      return "gemini"
    end
    if provider == "anthropic" or api == "anthropic-messages" then
      return "anthropic"
    end
    if api == "openai-responses" then
      return "openai"
    end
    return "unsupported"
  end

  -- Effective model: the `override_model` state wins, else the current
  -- model. Returns id, is_override, snapshot (nil for overrides).
  local function effective_model()
    local override = dex.state.get(OVERRIDE_KEY)
    if type(override) == "string" and override ~= "" then
      return override, true, nil
    end
    local ok, current = pcall(dex.model.current)
    if not ok or type(current) ~= "table" then
      error("web: no model configured (set model: <provider>/<model>)")
    end
    return current.id, false, current
  end

  local function urlencode(text)
    return (string.gsub(text, "[^A-Za-z0-9_.~-]", function(c)
      return string.format("%%%02X", string.byte(c))
    end))
  end

  local function auth_headers(auth)
    local headers = { ["Content-Type"] = "application/json" }
    if type(auth.headers) == "table" then
      for k, v in pairs(auth.headers) do
        if type(k) == "string" and type(v) == "string" then
          headers[k] = v
        end
      end
    end
    return headers
  end

  local function check_response(res, what)
    if type(res) ~= "table" then
      error("web: " .. what .. " failed (no response)")
    end
    if res.status ~= 200 then
      local detail = ""
      if type(res.body) == "string" and res.body ~= "" then
        detail = ": " .. string.sub(res.body, 1, 300)
      end
      error("web: " .. what .. " failed (status " .. tostring(res.status) .. ")" .. detail)
    end
    local ok, data = pcall(dex.json.decode, res.body)
    if not ok or type(data) ~= "table" then
      error("web: " .. what .. " returned a non-JSON body")
    end
    return data
  end

  -- -- sources ----------------------------------------------------------
  local function normalize_url(url)
    url = string.gsub(url, "#.*$", "")
    url = string.gsub(url, "%s+$", "")
    if string.find(url, "://.*/") then
      url = string.gsub(url, "/+$", "")
    end
    return url
  end

  local function push_source(sources, seen, url, title)
    if type(url) ~= "string" or url == "" then
      return
    end
    url = normalize_url(url)
    if seen[url] then
      return
    end
    seen[url] = true
    sources[#sources + 1] = { url = url, title = title or url }
  end

  local function render(text, sources, failed, count)
    local parts = { text }
    if #sources > 0 then
      parts[#parts + 1] = "\nSources:"
      local shown = 0
      for _, s in ipairs(sources) do
        if shown >= count then
          break
        end
        shown = shown + 1
        parts[#parts + 1] = shown .. ". " .. s.title .. " - " .. s.url
      end
      if #sources > shown then
        parts[#parts + 1] = "(" .. (#sources - shown) .. " more sources omitted)"
      end
    end
    if failed and #failed > 0 then
      parts[#parts + 1] = "\nFailed:"
      for _, f in ipairs(failed) do
        parts[#parts + 1] = "- " .. f
      end
    end
    local full = table.concat(parts, "\n")
    if #full > MAX_TEXT then
      return string.sub(full, 1, MAX_TEXT)
        .. "\n… (" .. (#full - MAX_TEXT) .. " chars truncated)"
    end
    return full
  end

  -- -- provider families -------------------------------------------------
  local function gemini_search(auth, model, query)
    local res = dex.net.fetch({
      url = auth.base_url .. "/v1beta1/models/" .. urlencode(model)
        .. ":generateContent?key=" .. auth.api_key,
      method = "POST",
      timeout_ms = NET_TIMEOUT_MS,
      headers = auth_headers(auth),
      body = dex.json.encode({
        contents = { { parts = { { text = query } } } },
        tools = { { google_search = {} } },
        generationConfig = { maxOutputTokens = 2048 },
      }),
    })
    local data = check_response(res, "search")
    local texts = {}
    local sources, seen = {}, {}
    local candidate = ((data.candidates or {})[1] or {}).content or {}
    for _, part in ipairs(candidate.parts or {}) do
      if type(part.text) == "string" then
        texts[#texts + 1] = part.text
      end
    end
    for _, chunk in ipairs(((data.groundingMetadata or {}).groundingChunks) or {}) do
      local web = chunk.web or {}
      push_source(sources, seen, web.uri, web.title)
    end
    return table.concat(texts, "\n"), sources
  end

  local function gemini_fetch_url(auth, model, url, query)
    local prompt = (query ~= "" and query .. "\n\n" or "")
      .. "Fetch and summarize the content at this URL, grounding factual claims: " .. url
    local res = dex.net.fetch({
      url = auth.base_url .. "/v1beta1/models/" .. urlencode(model)
        .. ":generateContent?key=" .. auth.api_key,
      method = "POST",
      timeout_ms = NET_TIMEOUT_MS,
      headers = auth_headers(auth),
      body = dex.json.encode({
        contents = { { parts = { { text = prompt } } } },
        tools = { { url_context = {} } },
        generationConfig = { maxOutputTokens = 2048 },
      }),
    })
    local data = check_response(res, "fetch")
    local texts = {}
    local sources, seen = {}, {}
    local candidate = ((data.candidates or {})[1] or {}).content or {}
    for _, part in ipairs(candidate.parts or {}) do
      if type(part.text) == "string" then
        texts[#texts + 1] = part.text
      end
    end
    for _, chunk in ipairs(((data.groundingMetadata or {}).groundingChunks) or {}) do
      local web = chunk.web or {}
      push_source(sources, seen, web.uri, web.title)
    end
    push_source(sources, seen, url, nil)
    return table.concat(texts, "\n"), sources
  end

  local function openai_search(auth, model, query)
    local headers = auth_headers(auth)
    headers["Authorization"] = "Bearer " .. auth.api_key
    local res = dex.net.fetch({
      url = auth.base_url .. "/responses",
      method = "POST",
      timeout_ms = NET_TIMEOUT_MS,
      headers = headers,
      body = dex.json.encode({
        model = model,
        input = query,
        tools = { { type = "web_search" } },
      }),
    })
    local data = check_response(res, "search")
    local texts = {}
    local sources, seen = {}, {}
    for _, item in ipairs(data.output or {}) do
      if item.type == "message" then
        for _, block in ipairs(item.content or {}) do
          if block.type == "output_text" then
            texts[#texts + 1] = block.text or ""
            for _, ann in ipairs(block.annotations or {}) do
              if ann.type == "url_citation" then
                push_source(sources, seen, ann.url, ann.title)
              end
            end
          end
        end
      end
    end
    return table.concat(texts, "\n"), sources
  end

  local function anthropic_search(auth, model, query)
    local headers = auth_headers(auth)
    headers["x-api-key"] = auth.api_key
    headers["anthropic-version"] = "2023-06-01"
    local res = dex.net.fetch({
      url = auth.base_url .. "/v1/messages",
      method = "POST",
      timeout_ms = NET_TIMEOUT_MS,
      headers = headers,
      body = dex.json.encode({
        model = model,
        max_tokens = 2048,
        tools = { { type = "web_search_20260209", name = "web_search" } },
        messages = { { role = "user", content = query } },
      }),
    })
    local data = check_response(res, "search")
    local texts = {}
    local sources, seen = {}, {}
    for _, block in ipairs(data.content or {}) do
      if block.type == "text" then
        texts[#texts + 1] = block.text or ""
        for _, cite in ipairs(block.citations or {}) do
          push_source(sources, seen, cite.url, cite.document_title)
        end
      end
    end
    return table.concat(texts, "\n"), sources
  end

  local function unsupported(id)
    error("web: model '" .. id .. "' has no search API "
      .. "(supported: gemini, openai-responses, anthropic-messages). "
      .. "Set '" .. OVERRIDE_KEY .. "' in dex.state or /model to a supported one.")
  end

  -- -- visibility --------------------------------------------------------
  -- Gemini serves both tools; every other supported family serves search
  -- only. Synced on `model_select` (fired on every first turn, and on every
  -- switch after) — never at load, where host calls are unavailable. The
  -- tools enforce the same gate at call time, so a stale slice fails
  -- loudly instead of mis-serving.
  local function sync_visibility()
    local ok, current = pcall(dex.model.current)
    if not ok or type(current) ~= "table" then
      return
    end
    if provider_kind(current.provider, current.api) == "gemini" then
      dex.tools.set_active({ SEARCH_TOOL, FETCH_TOOL })
    else
      dex.tools.set_active({ SEARCH_TOOL })
    end
  end

  -- -- tools --------------------------------------------------------------
  dex.tools.register({
      name = "search",
      execute = function(ctx, args)
        local query = args.query
        if type(query) ~= "string" or query == "" then
          error("search: query must be a non-empty string")
        end
      local count = args.count or 5
      if type(count) ~= "number" or count < 1 then
        count = 5
      end
      local id, _, current = effective_model()
      local model = current and current.model or id
      local kind = current and provider_kind(current.provider, current.api) or "unsupported"
      local auth = dex.model.auth()
      local text, sources
      if kind == "gemini" then
        text, sources = gemini_search(auth, model, query)
      elseif kind == "openai" then
        text, sources = openai_search(auth, model, query)
      elseif kind == "anthropic" then
        text, sources = anthropic_search(auth, model, query)
      else
        unsupported(id)
      end
      if text == "" then
        text = "(no answer text returned)"
      end
      return render(text, sources, nil, count)
    end,
  })

  dex.tools.register({
      name = "fetch",
      execute = function(ctx, args)
        if type(args.urls) ~= "table" or #args.urls == 0 then
          error("fetch: urls must be a non-empty array of strings")
        end
        local query = args.query or ""
        local id, _, current = effective_model()
        if not current or provider_kind(current.provider, current.api) ~= "gemini" then
          error("fetch: model '" .. id .. "' cannot fetch URLs (Gemini models only)")
        end
      local auth = dex.model.auth()
      local texts, sources, failed = {}, {}, {}
      local seen = {}
      for _, url in ipairs(args.urls) do
        if type(url) ~= "string" or url == "" then
          failed[#failed + 1] = "(empty url skipped)"
        else
          local ok, text, page_sources = pcall(gemini_fetch_url, auth, current.model, url, query)
          if ok then
            texts[#texts + 1] = "## " .. url .. "\n" .. text
            for _, s in ipairs(page_sources) do
              push_source(sources, seen, s.url, s.title)
            end
          else
            failed[#failed + 1] = url .. " (" .. tostring(text) .. ")"
          end
        end
      end
      return render(table.concat(texts, "\n\n"), sources, failed, #args.urls)
    end,
  })

  dex.events.on("model_select", function(ctx, ev)
    sync_visibility()
  end)
end
