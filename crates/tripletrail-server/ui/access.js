// The access pages (#205): who may read and write each dataset, as sparql-guard policies kept
// in the dataset's Turtle policy file, through the access admin API (/$/access). Plain
// JavaScript like app.js, which calls TripleTrailAccess.render(ui, args).
//
// /$/ui/access             overview: datasets, keys, groups
// /$/ui/access/{dataset}   the graph access matrix of one dataset: rows are graphs, columns
//                          are policies (who), cells are none / read / write
"use strict";

window.TripleTrailAccess = (function () {
  const SG = "https://matdata.eu/ns/sparql-guard#";
  const ANYONE = SG + "Anyone";
  const SIGNED_IN = SG + "AuthenticatedAgent";
  const DEFAULT_GRAPH = "urn:x-arq:DefaultGraph";
  let ui = null;
  const el = (...a) => ui.el(...a);
  const page = () => document.getElementById("page-access");
  const fill = (node, ...children) => node.replaceChildren(...children.flat().filter((c) => c != null && c !== false));

  /** Who a policy is for, in words. */
  function who(p) {
    const agents = p.agents.map((a) => (a === ANYONE ? "Anyone" : a === SIGNED_IN ? "Signed-in users" : a.replace(/^.*[/#:]/, "")));
    const groups = p.groups.map((g) => "group " + g);
    return [...agents, ...groups].join(", ") || "nobody";
  }
  function badges(p) {
    const out = [];
    if (p.writeAllGraphs) out.push(el("span", { class: "access-badge write" }, "writes all"));
    else if (p.readAllGraphs) out.push(el("span", { class: "access-badge read" }, "reads all"));
    if (p.readGraphs.length) out.push(el("span", { class: "access-badge read" }, `reads ${p.readGraphs.length} graph${p.readGraphs.length > 1 ? "s" : ""}`));
    if (p.writeGraphs.length) out.push(el("span", { class: "access-badge write" }, `writes ${p.writeGraphs.length} graph${p.writeGraphs.length > 1 ? "s" : ""}`));
    return out;
  }
  /** Whether an answer is a proposal (auth.review, #207): the change waits for another admin. */
  const proposed = (r) => r && typeof r === "object" && r.status === "pending" && r.proposal != null;
  const PROPOSED = "Proposed: another admin approves it under Proposals on the Access page.";
  const table = (caption, head, rows) =>
    el("table", { class: "grid" }, el("caption", {}, caption),
      el("thead", {}, el("tr", {}, head.map((h) => el("th", { scope: "col" }, h)))),
      el("tbody", {}, rows));

  // ---------- overview ----------
  async function overview() {
    const o = await ui.api("GET", "/$/access");
    const warnings = [];
    if (!o.policiesOn) warnings.push("No dataset has policies yet: every identified caller reads every dataset. Once one dataset has policies, the others are closed until they get theirs.");
    const closed = o.datasets.filter((d) => d.closed).map((d) => d.name);
    if (closed.length) warnings.push(`Closed to all but admins (no policies apply): ${closed.join(", ")}.`);
    const anonymousRowRules = o.datasets.some((d) => d.policies.some((p) => p.agents.includes(ANYONE)));
    if (anonymousRowRules && !o.anonymous) warnings.push("A policy is for Anyone, but anonymous access is off in the configuration: it applies to every identified caller only.");
    fill(page(),
      el("h2", {}, "Access"),
      el("p", { class: "muted" }, "Who may read and write each dataset. Changes are written as sparql-guard policies in ", el("code", {}, o.configFile), "'s policy files and ",
        o.review ? "apply when a second admin approves them." : "apply at once."),
      warnings.map((w) => el("p", { class: "access-warning", role: "note" }, w)),
      o.review ? await proposalsSection() : null,
      table("Datasets", ["Dataset", "Policies", "Files", ""], o.datasets.map((d) =>
        el("tr", {},
          el("th", { scope: "row" }, el("a", { href: "/$/ui/access/" + encodeURIComponent(d.name), "data-route": true }, d.name)),
          el("td", {}, d.closed ? el("span", { class: "access-badge bad" }, "closed") :
            d.policies.length ? el("ul", { class: "access-list" }, d.policies.map((p) => el("li", {}, who(p), " ", badges(p)))) : el("span", { class: "muted" }, "open (no policies)")),
          el("td", { class: "small" }, d.ownPolicies ? d.files.join(", ") : el("span", { class: "muted" }, "default policies")),
          el("td", {}, el("a", { href: "/$/ui/access/" + encodeURIComponent(d.name), "data-route": true, class: "btn small" }, "Edit access"))))),
      anonymousSwitch(o),
      table("API keys", ["Label", "Agent", "Groups", "Datasets", "Expires", "Last used", "Id", ""], o.keys.map((k) =>
        el("tr", {},
          el("td", {}, k.label || el("span", { class: "muted" }, "–")),
          el("td", { class: "small" }, k.agent),
          el("td", {}, k.groups.join(", ")),
          el("td", {}, k.datasets ? k.datasets.join(", ") : "all"),
          el("td", {}, k.expires ? [k.expires, k.expired ? el("span", { class: "access-badge bad" }, " expired") : null] : "never"),
          el("td", { class: "small" }, k.lastUsed || el("span", { class: "muted" }, "not since start")),
          el("td", {}, el("code", {}, k.id)),
          el("td", {}, k.builtIn ? el("span", { class: "muted small" }, "command line") :
            el("button", { type: "button", class: "btn small", "aria-label": "Revoke key " + (k.label || k.id), onclick: () => revoke(k) }, "Revoke"))))),
      el("p", { class: "muted small" }, "Keys are kept in the configuration file as SHA-256 hashes only, so a new key is shown once."),
      newKeyForm(o),
      table("Groups", ["Group", "Keys", "Policies"], o.groups.map((g) =>
        el("tr", {},
          el("th", { scope: "row" }, g.name, g.server ? el("span", { class: "access-badge" }, " server") : null),
          el("td", {}, String(g.keys)),
          el("td", { class: "small" }, g.policies.join(", ") || "–")))),
      o.trustedHeaders ? el("p", { class: "muted small" }, `Groups also come from the ${o.trustedHeaders.groups || "(no)"} header of the authenticating proxy.`) : null,
      await historySection());
  }

  // ---------- change history ----------
  /** A line diff of two texts: rows of [" " | "-" | "+", line]. */
  function diffLines(a, b) {
    const lines = (t) => (t === "" ? [] : t.split("\n"));
    const x = lines(a), y = lines(b);
    const n = x.length, m = y.length;
    if (n * m > 4e6) return [["-", `(${n} lines)`], ["+", `(${m} lines)`]];
    const lcs = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
    for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) lcs[i][j] = x[i] === y[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    const out = [];
    let i = 0, j = 0;
    while (i < n || j < m) {
      if (i < n && j < m && x[i] === y[j]) { out.push([" ", x[i]]); i++; j++; }
      else if (i < n && (j === m || lcs[i + 1][j] >= lcs[i][j + 1])) out.push(["-", x[i++]]);
      else out.push(["+", y[j++]]);
    }
    return out;
  }
  /** The changed lines with two lines around them. */
  function diffView(rows) {
    const near = (k) => rows.slice(Math.max(0, k - 2), k + 3).some((r) => r[0] !== " ");
    const shown = [];
    rows.forEach((r, k) => { if (near(k)) shown.push(r); else if (shown.length && shown[shown.length - 1] !== null) shown.push(null); });
    return el("pre", { class: "access-diff" }, shown.map((r) => r === null ? el("span", { class: "muted" }, "…\n") :
      el("span", { class: r[0] === "+" ? "add" : r[0] === "-" ? "del" : "" }, r[0] + " " + r[1] + "\n")));
  }
  /** The diffs from the files `from` to the files `to` (lists of {path, text}). */
  function fileDiffs(from, to) {
    const old = new Map((from || []).map((f) => [f.path, f.text]));
    const parts = [];
    for (const f of to) {
      const was = old.get(f.path);
      old.delete(f.path);
      if (was === f.text) continue;
      parts.push(el("h4", {}, f.path, was == null ? " (new)" : ""), diffView(diffLines(was || "", f.text)));
    }
    for (const [path] of old) parts.push(el("h4", {}, path, " (no longer named)"));
    return parts;
  }
  async function showVersion(v, target) {
    const s = await ui.api("GET", "/$/access/history/" + v.version);
    const changed = fileDiffs(s.before && s.before.files, s.files);
    // API keys stay as they are on a restore: the configuration is compared without them.
    const now = fileDiffs(s.current, s.files);
    fill(target,
      el("h3", {}, `Version ${s.version}: ${s.change}`),
      changed.length ? changed : el("p", { class: "muted" }, s.before ? "No change to the files." : "The files as they were found."),
      el("h3", {}, "What a restore of it would change now"),
      now.length ? [el("p", { class: "muted small" }, "API keys are left as they are now, whatever this shows for apiKeys."), now] : el("p", { class: "muted" }, "Nothing: the files are as they were then."));
  }
  // ---------- proposals (auth.review, #207) ----------
  async function proposalsSection() {
    let list;
    try { list = await ui.api("GET", "/$/access/proposals"); } catch (e) { return null; }
    const out = el("div", { id: "access-new-key-approved" });
    const decide = async (p, how) => {
      try {
        const r = await ui.api("POST", `/$/access/proposals/${p.proposal}/${how}`);
        ui.toast(`Proposal ${p.proposal} ${how === "approve" ? "approved and made" : "rejected"}.`, "good");
        await overview();
        if (r.key) {
          document.getElementById("access-new-key-approved").replaceChildren(
            el("p", { class: "access-warning", role: "alert" }, "The approved key, shown once: copy it now and hand it to its holder. "),
            el("p", {}, el("code", { class: "access-key" }, r.key)));
        }
      } catch (e) { ui.fail(e); }
    };
    const pending = list.proposals.filter((p) => p.status === "pending");
    const decided = list.proposals.filter((p) => p.status !== "pending").slice(0, 10);
    return [
      table("Proposals waiting for a second admin", ["#", "When", "By", "Change", ""], pending.length ? pending.map((p) =>
        el("tr", {},
          el("td", {}, String(p.proposal)),
          el("td", { class: "small" }, p.time),
          el("td", { class: "small" }, p.by),
          el("td", {}, p.description, el("details", {}, el("summary", { class: "small" }, "Details"), el("pre", { class: "small" }, JSON.stringify(p.change, null, 2)))),
          el("td", {},
            el("button", { type: "button", class: "btn small primary", onclick: () => decide(p, "approve") }, "Approve"), " ",
            el("button", { type: "button", class: "btn small", onclick: () => decide(p, "reject") }, "Reject")))) :
        [el("tr", {}, el("td", { colspan: 5, class: "muted" }, "None."))]),
      decided.length ? el("p", { class: "muted small" }, "Lately: ", decided.map((p) => `#${p.proposal} ${p.status} by ${p.decidedBy}`).join("; ")) : null,
      out];
  }

  async function historySection() {
    let h;
    try { h = await ui.api("GET", "/$/access/history"); } catch (e) { return null; }
    if (!h.versions.length) return el("p", { class: "muted small" }, "Changes made here are kept in a history, with what changed and who changed it, and can be undone.");
    const detail = el("div", {});
    const restore = async (v) => {
      const answer = await ui.confirmDialog(`Restore version ${v.version}`, "The configuration and policy files go back to how they were then, and apply at once. API keys stay as they are now. The restore is kept as a new version, so it can be undone too.");
      if (!answer.ok) return;
      try {
        const r = await ui.api("POST", `/$/access/history/${v.version}/restore`);
        ui.toast(proposed(r) ? PROPOSED : `Version ${v.version} restored.`, "good");
        await overview();
      } catch (e) { ui.fail(e); }
    };
    return [table("History", ["Version", "When", "By", "Change", ""], h.versions.map((v, k) =>
      el("tr", {},
        el("td", {}, String(v.version)),
        el("td", { class: "small" }, v.time),
        el("td", { class: "small" }, v.by || "–"),
        el("td", {}, v.change),
        el("td", {}, el("button", { type: "button", class: "btn small", onclick: () => showVersion(v, detail).catch(ui.fail) }, "Show"), " ",
          k > 0 ? el("button", { type: "button", class: "btn small", onclick: () => restore(v) }, "Restore") : null)))),
      el("p", { class: "muted small" }, `The last ${h.keep} versions are kept in .access-history next to the configuration.`),
      detail];
  }

  /** The switch for anonymous access (open data). */
  function anonymousSwitch(o) {
    const box = el("input", { type: "checkbox", checked: o.anonymous, onchange: async () => {
      box.disabled = true;
      try {
        const r = await ui.api("PUT", "/$/access/anonymous", { body: JSON.stringify({ anonymous: box.checked }), type: "application/json" });
        if (proposed(r)) ui.toast(PROPOSED, "good");
        await overview();
      } catch (e) {
        box.checked = !box.checked;
        box.disabled = false;
        alert("Not changed: " + e.message);
      }
    } });
    return el("p", { class: "access-row" }, el("label", {}, box, " Anonymous access: callers without a key or sign-in are let in, and see what policies for Anyone allow"));
  }

  async function revoke(k) {
    if (!confirm(`Revoke the key ${k.label || k.id}? Callers using it get 401 at once.`)) return;
    try {
      await ui.api("DELETE", "/$/access/keys/" + encodeURIComponent(k.id));
      await overview();
    } catch (e) {
      alert("Not revoked: " + e.message);
    }
  }

  /** The form that creates an API key; the key is shown once, then only its hash is kept. */
  function newKeyForm(o) {
    const input = (label, attrs) => {
      const i = el("input", Object.assign({ type: "text" }, attrs));
      return [i, el("label", {}, label, " ", i)];
    };
    const [label, labelField] = input("Label", { placeholder: "asset team" });
    const [agent, agentField] = input("Agent IRI", { placeholder: "https://example.org/people/…", required: true, size: 36 });
    const [groups, groupsField] = input("Groups", { placeholder: "network-readers, …", list: "access-groups" });
    const [datasets, datasetsField] = input("Datasets", { placeholder: "all" });
    const [expires, expiresField] = input("Expires", { type: "date" });
    const out = el("div", { role: "status" });
    const list = (v) => v.split(",").map((x) => x.trim()).filter(Boolean);
    async function create(event) {
      event.preventDefault();
      const body = { agent: agent.value.trim(), groups: list(groups.value) };
      if (label.value.trim()) body.label = label.value.trim();
      if (list(datasets.value).length) body.datasets = list(datasets.value);
      if (expires.value) body.expires = expires.value;
      try {
        const r = await ui.api("POST", "/$/access/keys", { body: JSON.stringify(body), type: "application/json" });
        await overview();
        if (proposed(r)) {
          document.getElementById("access-new-key").replaceChildren(el("p", { class: "muted", role: "status" },
            "A key for an admin is reviewed: the admin who approves it gets the key, once, to hand over."));
          return;
        }
        const shown = el("code", { class: "access-key" }, r.key);
        document.getElementById("access-new-key").replaceChildren(
          el("p", { class: "access-warning", role: "alert" }, "Copy the key now: it is not stored and will not be shown again. "),
          el("p", {}, shown, " ", el("button", { type: "button", class: "btn small", onclick: () => navigator.clipboard && navigator.clipboard.writeText(r.key) }, "Copy")));
      } catch (e) {
        fill(out, el("p", { class: "error" }, "Not created: " + e.message));
      }
    }
    // The new key goes below the form, which the page drew again (closed) after creating it.
    return [el("details", {}, el("summary", {}, "Create an API key"),
      el("datalist", { id: "access-groups" }, o.groups.map((g) => el("option", { value: g.name }))),
      el("form", { class: "access-row", onsubmit: create }, labelField, agentField, groupsField, datasetsField, expiresField,
        el("button", { type: "submit", class: "btn primary" }, "Create")),
      out),
      el("div", { id: "access-new-key" })];
  }

  /** "View as": open the query page of `name` as another caller, to check what they see. */
  function viewAsForm(name, policies) {
    const agent = el("input", { type: "text", size: 36, placeholder: "agent IRI; empty for anonymous", "aria-label": "Agent IRI to view as" });
    const groups = el("input", { type: "text", placeholder: "groups, comma separated", "aria-label": "Groups to view as", list: "view-as-groups" });
    const known = [...new Set(policies.flatMap((p) => p.groups))];
    const open = (event) => {
      event.preventDefault();
      const g = groups.value.split(",").map((x) => x.trim()).filter(Boolean);
      if (!agent.value.trim() && g.length) { ui.toast("An anonymous caller has no groups: give an agent IRI.", "bad"); return; }
      ui.viewAs.set({ agent: agent.value.trim() || null, groups: g });
      ui.go("query/" + encodeURIComponent(name));
    };
    return el("details", {}, el("summary", {}, "View as someone else"),
      el("p", { class: "muted small" }, "Opens the query page with that caller's access to every dataset, until you stop it in the banner. Saved changes apply at once."),
      el("datalist", { id: "view-as-groups" }, known.map((g) => el("option", { value: g }))),
      el("form", { class: "access-row", onsubmit: open }, agent, groups, el("button", { type: "submit", class: "btn" }, "Open the query page as them")));
  }

  // ---------- the graph access matrix of one dataset ----------
  /** The level of policy `p` on graph `g`: "write", "read" or "". */
  function level(p, g) {
    if (g === "*") return p.writeAllGraphs ? "write" : p.readAllGraphs ? "read" : "";
    if (p.writeGraphs.includes(g)) return "write";
    if (p.readGraphs.includes(g)) return "read";
    return "";
  }
  function setLevel(p, g, v) {
    if (g === "*") { p.writeAllGraphs = v === "write"; p.readAllGraphs = v === "read" || v === "write"; return; }
    p.readGraphs = p.readGraphs.filter((x) => x !== g);
    p.writeGraphs = p.writeGraphs.filter((x) => x !== g);
    if (v === "read") p.readGraphs.push(g);
    if (v === "write") p.writeGraphs.push(g);
  }
  const slug = (s) => s.replace(/^.*[/#:]/, "").replace(/[^A-Za-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "") || "policy";

  async function matrix(name) {
    const [o, g] = await Promise.all([ui.api("GET", "/$/access"), ui.api("GET", `/$/access/datasets/${encodeURIComponent(name)}/graphs`)]);
    const d = o.datasets.find((x) => x.name === name);
    if (!d) { fill(page(), el("p", { class: "error" }, `No dataset ${name}.`)); return; }
    // Working copies; saved policy by policy.
    const policies = d.policies.filter((p) => p.id).map((p) => JSON.parse(JSON.stringify(p)));
    const saved = new Map(policies.map((p) => [p.id, JSON.stringify(p)]));
    const graphs = [{ graph: "*", label: "All graphs" }, { graph: DEFAULT_GRAPH, label: "Default graph" }, ...g.graphs];
    const status = el("p", { class: "muted small", role: "status" });
    const isEmpty = (p) => !p.readAllGraphs && !p.writeAllGraphs && !p.readGraphs.length && !p.writeGraphs.length;
    // A column never saved counts once it grants something; an empty one has nothing to save.
    const unsaved = () => policies.filter((p) => (saved.has(p.id) ? saved.get(p.id) !== JSON.stringify(p) : !isEmpty(p)));
    const render = () => {
      const changed = unsaved();
      status.textContent = changed.length ? `${changed.length} unsaved change${changed.length > 1 ? "s" : ""}` : "Saved";
      fill(grid, el("caption", {}, `Graph access of ${name}`),
        el("thead", {}, el("tr", {}, el("th", { scope: "col" }, "Graph"), el("th", { scope: "col" }, "Triples"),
          policies.map((p) => el("th", { scope: "col", title: p.id }, who(p))))),
        el("tbody", {}, graphs.map((row) => el("tr", {},
          el("th", { scope: "row" }, row.label ? [row.label, el("br"), el("span", { class: "muted small" }, row.graph === "*" ? "" : row.graph)] : row.graph),
          el("td", { class: "access-num" }, row.triples != null ? ui.fmtNum(row.triples) : ""),
          policies.map((p) => el("td", {}, el("select", {
            "aria-label": `${who(p)} on ${row.label || row.graph}`,
            onchange: (e) => { setLevel(p, row.graph, e.target.value); render(); },
          }, ["", "read", "write"].map((v) => {
            const o = el("option", { value: v }, v || "none");
            o.selected = level(p, row.graph) === v;
            return o;
          }))))))));
    };
    const grid = el("table", { class: "grid matrix" });
    const principal = el("input", { type: "text", placeholder: "group name, agent IRI, anyone or signed-in", "aria-label": "Who the new column is for" });
    const addColumn = () => {
      const v = principal.value.trim();
      if (!v) return;
      const p = { id: "", agents: [], groups: [], readAllGraphs: false, writeAllGraphs: false, readGraphs: [], writeGraphs: [] };
      if (v.toLowerCase() === "anyone") p.agents.push(ANYONE);
      else if (v.toLowerCase() === "signed-in") p.agents.push(SIGNED_IN);
      else if (/^[a-z][a-z0-9+.-]*:/i.test(v) && !/\s/.test(v) && v.includes(":") && !v.startsWith("tripletrail:")) p.agents.push(v);
      else p.groups.push(v);
      p.id = `urn:tripletrail:access:${encodeURIComponent(name)}:${slug(v)}`;
      if (policies.some((x) => x.id === p.id)) { ui.toast("There is a column for that already."); return; }
      policies.push(p);
      principal.value = "";
      render();
    };
    const prefix = el("input", { type: "text", placeholder: "urn:network:release:", "aria-label": "Graph prefix" });
    const column = el("select", { "aria-label": "Column" });
    const bulkLevel = el("select", { "aria-label": "Level" }, ["read", "write", ""].map((v) => el("option", { value: v }, v || "none")));
    const refreshColumns = () => fill(column, policies.map((p, i) => el("option", { value: i }, who(p))));
    const bulk = () => {
      const p = policies[column.value];
      if (!p || !prefix.value) return;
      const hits = g.graphs.filter((x) => x.graph.startsWith(prefix.value));
      for (const x of hits) setLevel(p, x.graph, bulkLevel.value);
      ui.toast(`${hits.length} graph${hits.length === 1 ? "" : "s"} set.`);
      render();
    };
    const save = async () => {
      const changed = unsaved();
      try {
        let proposals = 0;
        for (const p of changed) {
          const empty = isEmpty(p);
          let r = null;
          if (empty && saved.has(p.id)) r = await ui.api("DELETE", `/$/access/datasets/${encodeURIComponent(name)}/policies`, { params: { id: p.id } });
          else if (!empty) r = await ui.api("PUT", `/$/access/datasets/${encodeURIComponent(name)}/policies`, { body: JSON.stringify(p), type: "application/json" });
          if (proposed(r)) proposals++;
          else if (empty) saved.delete(p.id); else saved.set(p.id, JSON.stringify(p));
        }
        ui.toast(proposals ? `${proposals === 1 ? "The change is" : proposals + " changes are"} proposed: another admin approves ${proposals === 1 ? "it" : "them"} on the Access page.` : "Access saved.", "good");
      } catch (e) { ui.fail(e); }
      render();
    };
    const blank = d.policies.filter((p) => !p.id).length;
    fill(page(),
      el("p", {}, el("a", { href: "/$/ui/access", "data-route": true }, "← Access")),
      el("h2", {}, `Access to ${name}`),
      !d.editable ? el("p", { class: "access-warning" }, "These policies are not in Turtle (.ttl) files and cannot be edited here.") : null,
      !d.ownPolicies ? el("p", { class: "muted" }, "This dataset uses the default policies. Saving gives it its own policy file, starting as a copy of them.") : null,
      blank ? el("p", { class: "muted small" }, `${blank} polic${blank > 1 ? "ies" : "y"} without an IRI are not shown and stay as they are.`) : null,
      el("div", { class: "access-scroll" }, grid),
      el("div", { class: "access-row" }, principal, el("button", { type: "button", class: "btn", onclick: addColumn }, "Add column")),
      el("details", {}, el("summary", {}, "Grant many graphs at once"),
        el("div", { class: "access-row" }, el("label", {}, "Graphs starting with ", prefix), el("label", {}, " to ", column), el("label", {}, " as ", bulkLevel),
          el("button", { type: "button", class: "btn", onclick: bulk, onfocus: refreshColumns }, "Apply"))),
      el("div", { class: "access-row" }, el("button", { type: "button", class: "btn primary", onclick: save, disabled: !d.editable }, "Save"), status),
      viewAsForm(name, policies));
    refreshColumns();
    render();
  }

  async function render(shared, args) {
    ui = shared;
    try {
      if (args[0]) await matrix(decodeURIComponent(args[0]));
      else await overview();
    } catch (e) {
      fill(page(), el("p", { class: "error" }, e.status === 403 ? "The access pages need the group tripletrail:admin." :
        e.status === 404 ? e.message : "The access pages could not be loaded: " + e.message));
    }
  }
  return { render };
})();
