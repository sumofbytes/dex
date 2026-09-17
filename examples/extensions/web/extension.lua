-- web: provider-native web search + URL fetch, model-independent.
--
-- Reference extension for model-aware Lua extensions: it registers
-- provider-native tools (`search` wherever a target exists, `fetch` when a
-- Gemini target exists — the model sees them as `ext__web__search` /
-- `ext__web__fetch`; extension code uses the short names and the host
-- resolves them), reuses the current model's endpoint + key (`dex.model`),
-- falls back to a configured override provider (`/search-model`) when the
-- current model cannot serve search, hides tools no target can serve
-- (`model_select` + `dex.tools.set_active`), and never switches the served
-- model silently (cost safety — switch with /model).
--
-- Target: the current model when its family serves search, else the
-- `override_model` (dex.state, set with /search-model) — the extension
-- calls that provider's configured endpoint with that provider's own
-- key (`dex.model.auth(provider)` + the `net.providers` capability),
-- so search works even when the served model has no search API. Never
-- both: a served attempt bills, so a transient failure on the current
-- model errors instead of retrying cross-provider (cost safety).
--
-- Copy this directory to `$XDG_CONFIG_HOME/dex/extensions/web`
-- (or `dex extensions install <dir>`) to use it.

return function(dex)
  local SEARCH_TOOL = "search"
  local FETCH_TOOL = "fetch"
  local MAX_TEXT = 8000
  -- Anthropic versions its server-side search tool (`web_search_YYYYMMDD`);
  -- the server rejects unknown versions loudly, so bump this when they
  -- publish a newer one.
  local ANTHROPIC_WEB_SEARCH_TOOL = "web_search_20260209"
  -- Provider-side search/grounding rounds run ~40s+ non-streamed; keep
  -- this under the manifest per-tool `timeout` (120s).
  local NET_TIMEOUT_MS = 100000
  -- dex.state key holding the override model as "provider/model" (false =
  -- unset; dex.state has no delete, so `off` writes false).
  local OVERRIDE_KEY = "override_model"

    -- Which provider family serves a model. Gemini speaks its own search +
    -- url_context tools; OpenAI-Responses and Anthropic-Messages wires carry
    -- a web_search tool; anything else is unsupported (and the tool is then
    -- hidden, not offered-and-broken). Order matters: the codex deny comes
    -- first (it rides the OpenAI wire but its backend-api serves no search
    -- tool), Gemini stays name-first (generics default to the
    -- openai-responses wire string even when their endpoint speaks the
    -- Gemini API), and otherwise the wire wins over the name — a re-pinned
    -- provider (e.g. anthropic on openai-responses) serves its wire's tool.
    local function provider_kind(provider, api)
      provider = string.lower(provider or "")
      api = api or ""
      if provider == "openai-codex" or provider == "codex" then
        return "unsupported"
      end
      if provider == "gemini" or provider == "google" then
        return "gemini"
      end
      if api == "anthropic-messages" then
        return "anthropic"
      end
      if api == "openai-responses" then
        return "openai"
      end
      if provider == "anthropic" then
        return "anthropic"
      end
      return "unsupported"
    end

    -- Family for a *named* provider from its configured wire
    -- (`dex.model.auth(provider)` reports the provider's `api:` pin /
    -- default — a re-pin beats the name). Never errors — unknown or
    -- keyless providers simply don't become targets (the /search-model
    -- command surfaces the auth error itself, at set time).
    local function kind_for_provider(provider)
      local ok, auth = pcall(dex.model.auth, provider)
      if ok and type(auth) == "table" and type(auth.api) == "string" then
        return provider_kind(provider, auth.api)
      end
      if ok then
        return provider_kind(provider, nil)
      end
      return "unsupported"
    end

  -- Current model (never a silent switch: the extension serves whatever
  -- dex serves — change it with /model).
  local function current_model()
    local ok, current = pcall(dex.model.current)
    if not ok or type(current) ~= "table" then
      error("web: no model configured (set model: <provider>/<model>)", 0)
    end
    return current.id, current
  end

    -- Split + canonicalize an override value: trimmed model, lowercased
    -- provider (host resolution is case-insensitive, but target dedup
    -- compares exact strings — normalize once, here). One helper for both
    -- the reader and the /search-model writer.
    local function split_override(text)
      if type(text) ~= "string" then
        return nil
      end
      local provider, model = string.match(text, "^%s*([%w%-]+)/(.+)%s*$")
      if not provider then
        return nil
      end
      model = string.gsub(model, "%s+$", "")
      if model == "" then
        return nil
      end
      return string.lower(provider), model
    end

    local function parse_override()
      return split_override(dex.state.get(OVERRIDE_KEY))
    end

    -- The single serving target: the current model when its family serves
    -- search, else the override (never both — a served attempt bills, so a
    -- transient failure must not retry cross-provider; see the header).
    local function search_targets(current)
    local targets = {}
    local function add(kind, provider, model, origin, is_current)
      for _, t in ipairs(targets) do
        if t.provider == provider and t.model == model then
          return
        end
      end
      targets[#targets + 1] = {
        kind = kind,
        provider = provider,
        model = model,
        origin = origin,
        current = is_current,
      }
    end
      local ck = provider_kind(current.provider, current.api)
      if ck ~= "unsupported" then
        add(ck, current.provider, current.model, "current model " .. current.id, true)
        return targets
      end
      local provider, model = parse_override()
    if provider then
      local kind = kind_for_provider(provider)
      if kind ~= "unsupported" then
        add(kind, provider, model, "override " .. provider .. "/" .. model, false)
      end
    end
    return targets
  end

  local function auth_for(target)
    local ok, auth = pcall(dex.model.auth, target.current and nil or target.provider)
    if ok and type(auth) == "table" then
      return auth
    end
    return nil, tostring(auth)
  end

  -- Actionable guidance as a tool *result*, not an error: the agent reads
  -- it, relays the options, and does not retry.
  local function unavailable(id)
    return "web: no provider-native search is available for '" .. id .. "'.\n"
      .. "Served wire families with a search tool: gemini (google_search), "
      .. "openai-responses (web_search), anthropic-messages (web_search).\n"
      .. "Fixes, in preference order:\n"
      .. "1. /model <provider>/<model> — switch to a model with native search.\n"
      .. "2. /search-model <provider>/<model> — set an override; the extension then "
      .. "calls that provider's configured endpoint and key (the provider must exist "
      .. "under 'providers:' in config.yaml). Works across providers.\n"
      .. "3. /search-model off — clear an override.\n"
      .. "Do not retry search as-is; tell the user about the options instead."
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
      error("web: " .. what .. " failed (no response)", 0)
    end
    if res.status ~= 200 then
      local detail = ""
      if type(res.body) == "string" and res.body ~= "" then
        detail = ": " .. string.sub(res.body, 1, 300)
      end
      error("web: " .. what .. " failed (status " .. tostring(res.status) .. ")" .. detail, 0)
    end
    local ok, data = pcall(dex.json.decode, res.body)
    if not ok or type(data) ~= "table" then
      error("web: " .. what .. " returned a non-JSON body", 0)
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
        tools = { { type = ANTHROPIC_WEB_SEARCH_TOOL, name = "web_search" } },
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

  local FAMILY = {
    gemini = gemini_search,
    openai = openai_search,
    anthropic = anthropic_search,
  }

  -- One attempt against one target: run(auth, target.model, ...) on
  -- success, or a "<origin>: <why>" failure line.
  local function attempt(target, run, ...)
    local auth, auth_err = auth_for(target)
    if not auth then
      return nil, nil, target.origin .. ": " .. (auth_err or "no credentials")
    end
    local ok, a, b, c = pcall(run, auth, target.model, ...)
    if ok then
      return a, b, c
    end
    return nil, nil, target.origin .. ": " .. tostring(a)
  end

  -- -- visibility --------------------------------------------------------
    -- Tools are hidden when no target can serve them — a tool that always
    -- errors must never enter the model's schema. Synced on `model_select`
    -- (fired on every first turn, and on every switch after) and by the
    -- /search-model command itself (model_select only fires on change, so
    -- an override set under the same served model would otherwise stay
    -- hidden) — never at load, where host calls are unavailable. The tools
    -- enforce the same gate at call time, so a stale slice fails loudly
    -- instead of mis-serving.
  local function sync_visibility()
    local ok, current = pcall(dex.model.current)
    if not ok or type(current) ~= "table" then
      return
    end
    local ck = provider_kind(current.provider, current.api)
    local active = {}
    if ck ~= "unsupported" then
      active[#active + 1] = SEARCH_TOOL
    end
    if ck == "gemini" then
      active[#active + 1] = FETCH_TOOL
    end
    local provider = parse_override()
    if provider then
      local kind = kind_for_provider(provider)
      if kind == "gemini" then
        -- The override serves both tools even when the current model can
        -- serve neither.
        active = { SEARCH_TOOL, FETCH_TOOL }
      elseif kind ~= "unsupported" and ck == "unsupported" then
        active[#active + 1] = SEARCH_TOOL
      end
    end
    dex.tools.set_active(active)
  end

  -- -- tools --------------------------------------------------------------
  dex.tools.register({
    name = "search",
    execute = function(ctx, args)
      local query = args.query
      if type(query) ~= "string" or query == "" then
        error("search: query must be a non-empty string", 0)
      end
      local count = args.count or 5
      if type(count) ~= "number" or count < 1 then
        count = 5
      end
      local id, current = current_model()
      local targets = search_targets(current)
      if #targets == 0 then
        return unavailable(id)
      end
      local failures = {}
      for _, target in ipairs(targets) do
        local text, sources, failure = attempt(target, FAMILY[target.kind], query)
        if text then
          if text == "" then
            text = "(no answer text returned)"
          end
          return render(text, sources, nil, count)
        end
        failures[#failures + 1] = failure
      end
        -- A served failure is guidance, not an error — like fetch below
        -- and the no-target path above: the agent relays the options
        -- instead of retrying.
        return "web: search failed for '"
          .. id
          .. "' — "
          .. table.concat(failures, "; ")
          .. ".\n"
          .. unavailable(id)
    end,
  })

  dex.tools.register({
    name = "fetch",
    execute = function(ctx, args)
      if type(args.urls) ~= "table" or #args.urls == 0 then
        error("fetch: urls must be a non-empty array of strings", 0)
      end
      local query = args.query or ""
      local id, current = current_model()
      -- URL context is Gemini's url_context tool: current gemini, else a
      -- gemini override.
      local targets = {}
      for _, target in ipairs(search_targets(current)) do
        if target.kind == "gemini" then
          targets[#targets + 1] = target
        end
      end
      if #targets == 0 then
        return unavailable(id)
      end
      local texts, sources, failed = {}, {}, {}
      local seen = {}
      for _, url in ipairs(args.urls) do
        if type(url) ~= "string" or url == "" then
          failed[#failed + 1] = "(empty url skipped)"
        else
          local served = false
          local attempts = {}
          for _, target in ipairs(targets) do
            local text, page_sources, failure =
              attempt(target, gemini_fetch_url, url, query)
            if text then
              texts[#texts + 1] = "## " .. url .. "\n" .. text
              for _, s in ipairs(page_sources) do
                push_source(sources, seen, s.url, s.title)
              end
              served = true
              break
            end
            attempts[#attempts + 1] = failure
          end
          if not served then
            failed[#failed + 1] = url .. " (" .. table.concat(attempts, "; ") .. ")"
          end
        end
      end
      if #texts == 0 and #failed == #args.urls then
        -- Nothing worked at all: surface the guidance instead of a bare
        -- failure list.
        return unavailable(id) .. "\nFailed:\n- " .. table.concat(failed, "\n- ")
      end
      return render(table.concat(texts, "\n\n"), sources, failed, #args.urls)
    end,
  })

  -- -- /search-model: the user-facing override knob -----------------------
  dex.commands.register({
    name = "search-model",
    description = "web: set the search/fetch override model (<provider>/<model>, or 'off')",
    execute = function(ctx, arg)
      local text = string.gsub(string.gsub(arg or "", "^%s+", ""), "%s+$", "")
        if text == "" or string.lower(text) == "off" or string.lower(text) == "false" then
          dex.state.set(OVERRIDE_KEY, false)
          sync_visibility()
          return "web: search override cleared — search serves the current model only"
        end
        local provider, model = split_override(text)
        if not provider then
          error(
            "web: /search-model needs '<provider>/<model>' (e.g. anthropic/claude-sonnet-4-5) or 'off'",
            0
          )
        end
        -- Validate now, not at search time: the provider must exist (its
        -- auth error says where to declare it), and its wire must actually
        -- carry a search tool.
        local ok, auth = pcall(dex.model.auth, provider)
        if not ok then
          error("web: unknown search provider '" .. provider .. "': " .. tostring(auth), 0)
        end
        local kind = provider_kind(
          provider,
          type(auth) == "table" and type(auth.api) == "string" and auth.api or nil
        )
        if kind == "unsupported" then
        error(
          "web: provider '" .. provider
            .. "' has no provider-native search to override with (need gemini, "
            .. "anthropic, or an openai-responses provider)",
          0
        )
      end
        dex.state.set(OVERRIDE_KEY, provider .. "/" .. model)
        -- Re-sync here: model_select only fires on provider/model change,
        -- so without this the tools would stay hidden until a /model
        -- switch.
        sync_visibility()
        return "web: search override set to " .. provider .. "/" .. model
        .. " (family " .. kind .. ") — search/fetch now fall back to it "
        .. "when the current model has no native search"
    end,
  })

  dex.events.on("model_select", function(ctx, ev)
    sync_visibility()
  end)
end
