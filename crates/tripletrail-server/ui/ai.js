// The AI assistant's connection (#146): bring your own key, as MatGUI's assistant on the Query
// page does. The key, the provider and the model are the ones MatGUI keeps in this browser, so
// a key entered once works on both pages; nothing goes anywhere but to the chosen provider.
// Every provider is spoken to through its OpenAI-compatible `chat/completions`, with tools.
"use strict";
window.TripleTrailAI = (function () {
  /** Where MatGUI keeps its AI settings: the Query page's Yasgui is `persistenceId: "tripletrail-ui"`. */
  const STORE_KEY = "tripletrail-ui_ai";
  const NAMESPACE = "yasgui-ai";
  /** Providers offered when nothing is set yet; the same list as MatGUI's. */
  const PROVIDERS = [
    { name: "OpenAI", baseUrl: "https://api.openai.com/v1", model: "gpt-4.1-mini", keys: "https://platform.openai.com/api-keys" },
    { name: "Anthropic", baseUrl: "https://api.anthropic.com/v1", model: "claude-sonnet-5-5", keys: "https://console.anthropic.com/settings/keys" },
    { name: "Mistral (EU)", baseUrl: "https://api.mistral.ai/v1", model: "mistral-large-latest", keys: "https://console.mistral.ai/api-keys" },
    { name: "Scaleway (EU)", baseUrl: "https://api.scaleway.ai/v1", model: "llama-3.3-70b-instruct", keys: "https://console.scaleway.com/iam/api-keys" },
    { name: "Groq", baseUrl: "https://api.groq.com/openai/v1", model: "llama-3.3-70b-versatile", keys: "https://console.groq.com/keys" },
    { name: "OpenRouter", baseUrl: "https://openrouter.ai/api/v1", model: "", keys: "https://openrouter.ai/keys" },
    { name: "Ollama (this computer)", baseUrl: "http://localhost:11434/v1", model: "llama3.1", noKey: true },
    { name: "LM Studio (this computer)", baseUrl: "http://localhost:1234/v1", model: "", noKey: true },
  ];

  function hostOf(url) {
    try { return new URL(url).host; } catch (e) { return ""; }
  }

  /** MatGUI's saved settings, in its older one-service form too. */
  function readSettings() {
    let s = {};
    try {
      const raw = JSON.parse(localStorage.getItem(STORE_KEY) || "null");
      if (raw && raw.val && typeof raw.val === "object") s = raw.val;
    } catch (e) { /* none, or storage is off */ }
    if (s.baseUrl || s.apiKey || s.model) {
      const { service, baseUrl, apiKey, model, ...rest } = s;
      s = { ...rest, services: [...(rest.services || []), { id: "own-1", baseUrl, apiKey, model }] };
      if (service === "own") s.defaultService = "own-1";
      if (service === "off") s.defaultService = "off";
    }
    return s;
  }
  function writeSettings(s) {
    try {
      localStorage.setItem(STORE_KEY, JSON.stringify({ namespace: NAMESPACE, val: s, time: Date.now() / 1000 }));
    } catch (e) { throw new Error("This browser does not keep settings (private window?): the key can't be saved."); }
  }

  /** The services set up in this browser, each with its base URL, key, model and a label. */
  function connections() {
    return (readSettings().services || []).filter((x) => x && x.baseUrl).map((x) => ({
      id: x.id, baseUrl: String(x.baseUrl).replace(/\/+$/, ""), apiKey: x.apiKey || "", model: (x.model || "").trim(),
      host: hostOf(x.baseUrl), label: (x.name || "").trim() || hostOf(x.baseUrl),
    }));
  }
  /** The service in use: the one picked last, else MatGUI's default, else the first. */
  function connection(id) {
    const s = readSettings();
    if (s.defaultService === "off" && !id) return null;
    const all = connections();
    return all.find((c) => c.id === id) || all.find((c) => c.id === s.defaultService) || all[0] || null;
  }
  /** Adds or changes a service and makes it the default, for MatGUI too. */
  function saveConnection(c) {
    const s = readSettings();
    const services = (s.services || []).filter((x) => x.id !== c.id);
    const id = c.id || "own-" + Date.now().toString(36) + Math.random().toString(36).slice(2, 6);
    services.push({ id, name: c.name || undefined, baseUrl: c.baseUrl, apiKey: c.apiKey || undefined, model: c.model || undefined });
    writeSettings({ ...s, services, defaultService: id });
    return id;
  }
  function removeConnection(id) {
    const s = readSettings();
    writeSettings({ ...s, services: (s.services || []).filter((x) => x.id !== id), defaultService: s.defaultService === id ? undefined : s.defaultService });
  }

  function headers(c) {
    const h = { "Content-Type": "application/json" };
    if (c.apiKey && /\.(openai\.azure\.com|services\.ai\.azure\.com)$/.test(c.host)) h["api-key"] = c.apiKey;
    else if (c.apiKey) h.Authorization = "Bearer " + c.apiKey;
    if (c.host === "api.anthropic.com") {
      h["anthropic-dangerous-direct-browser-access"] = "true";
      h["anthropic-version"] = "2023-06-01";
      if (c.apiKey) h["x-api-key"] = c.apiKey;
    }
    return h;
  }

  /** One call: the model's next message (`content`, `tool_calls`). */
  async function complete(c, messages, tools, signal) {
    const body = { messages };
    if (c.model) body.model = c.model;
    if (tools && tools.length) body.tools = tools.map((t) => ({ type: "function", function: { name: t.name, description: t.description, parameters: t.parameters } }));
    let r;
    try {
      r = await fetch(c.baseUrl + "/chat/completions", { method: "POST", headers: headers(c), body: JSON.stringify(body), signal, credentials: "omit" });
    } catch (e) {
      if (e && e.name === "AbortError") throw e;
      throw new Error(`Could not reach ${c.host}. Check the base URL and your network; a local server must allow this page's origin (CORS).`);
    }
    const text = await r.text();
    if (!r.ok) {
      let detail = "";
      try { const j = JSON.parse(text); detail = (j.error && j.error.message) || j.message || j.detail || ""; } catch (e) { detail = text.slice(0, 300); }
      const what = r.status === 401 || r.status === 403 ? "The API key was refused" : r.status === 429 ? "The provider's rate limit or quota was hit" : r.status === 404 ? "The model or the base URL was not found" : "The AI service answered " + r.status;
      throw new Error(what + (detail ? ": " + String(detail) : "."));
    }
    const j = JSON.parse(text);
    const m = j.choices && j.choices[0] && j.choices[0].message;
    if (!m) throw new Error("The AI service gave no answer.");
    return m;
  }

  /**
   * Asks until the model answers without calling a tool, or `maxSteps` tool rounds pass.
   * `tools` are `{name, description, parameters, run(args)}`; `onStep(kind, detail)` tells the
   * page what happens. Returns the model's last text; `messages` keeps the conversation.
   */
  async function converse(c, messages, tools, { signal, maxSteps = 8, onStep } = {}) {
    for (let step = 0; ; step++) {
      const last = step >= maxSteps;
      const m = await complete(c, messages, last ? [] : tools, signal);
      const calls = !last && m.tool_calls && m.tool_calls.length ? m.tool_calls : null;
      // Past the cap no tool runs and nothing more is asked, whatever the model sends: each
      // request costs the user, and some servers call tools even when none are offered.
      messages.push({ role: "assistant", content: m.content || "", ...(calls ? { tool_calls: calls } : {}) });
      if (!calls) {
        if (m.content) return m.content;
        if (last) throw new Error(`The model did not finish within ${maxSteps} rounds of tool calls. Ask again, more narrowly.`);
        return "";
      }
      for (const call of calls) {
        const tool = tools.find((t) => t.name === call.function.name);
        let args = {};
        try { args = JSON.parse(call.function.arguments || "{}"); } catch (e) { /* the tool says what is missing */ }
        if (onStep) onStep("tool", { name: call.function.name, args });
        let out;
        try { out = tool ? await tool.run(args) : "No such tool: " + call.function.name; } catch (e) { out = "Error: " + e.message; }
        out = typeof out === "string" ? out : JSON.stringify(out);
        if (out.length > 30000) out = out.slice(0, 30000) + "\n… (cut at 30,000 characters)";
        messages.push({ role: "tool", tool_call_id: call.id, content: out });
      }
    }
  }

  /** An embedding model to start from, for the services that have one. */
  function embeddingModel(c) {
    const by = { "api.openai.com": "text-embedding-3-small", "api.mistral.ai": "mistral-embed", "api.scaleway.ai": "bge-multilingual-gemma2", "localhost:11434": "nomic-embed-text", "openrouter.ai": "openai/text-embedding-3-small" };
    return by[c.host] || "";
  }

  /**
   * The vector of `text` from the service's OpenAI-compatible `embeddings` (#219): the text goes
   * to the chosen provider only, never to TripleTrail's server, which is given the numbers.
   */
  async function embed(c, model, text, signal) {
    let r;
    try {
      r = await fetch(c.baseUrl + "/embeddings", { method: "POST", headers: headers(c), body: JSON.stringify({ model, input: text }), signal, credentials: "omit" });
    } catch (e) {
      if (e && e.name === "AbortError") throw e;
      throw new Error(`Could not reach ${c.host}. Check the base URL and your network; a local server must allow this page's origin (CORS).`);
    }
    const body = await r.text();
    if (!r.ok) {
      let detail = "";
      try { const j = JSON.parse(body); detail = (j.error && j.error.message) || j.message || j.detail || ""; } catch (e) { detail = body.slice(0, 300); }
      const what = r.status === 401 || r.status === 403 ? "The API key was refused" : r.status === 429 ? "The provider's rate limit or quota was hit" : r.status === 404 ? "This service or model has no embeddings" : "The AI service answered " + r.status;
      throw new Error(what + (detail ? ": " + String(detail) : "."));
    }
    let v = null;
    try { const j = JSON.parse(body); v = j.data && j.data[0] && j.data[0].embedding; } catch (e) { /* below */ }
    if (!Array.isArray(v) || !v.length || !v.every(Number.isFinite)) throw new Error("The AI service gave no vector.");
    return v;
  }

  return { PROVIDERS, connections, connection, saveConnection, removeConnection, converse, complete, embed, embeddingModel };
})();
