"use strict";

// The programmable metrics demo UI: an editor that compiles and publishes policy programs, and a viewer of what each
// endpoint receives. It polls /api/state about once per second and keeps no state the server does not have, apart from
// the editor's text, which is also kept as a draft per default program in the browser's storage.
(() => {
  const POLL_MS = 1000;
  const LIST_WINDOW_MS = 5000; // families seen in the last 5 s are listed...
  const FADE_AFTER_MS = 3000; // ...and fade after 3 s without points.
  const ARRIVED_MS = 8000; // a family that began arriving within 8 s is marked new
  // Payloads arrive every second or two, so rates average over at least two seconds, and over up to twelve once a
  // family has been arriving that long; sparklines average over four, a multiple of the flush cadence, so that a
  // steady flow draws flat.
  const RATE_MIN_S = 2;
  const RATE_MAX_S = 12;
  const SMOOTH_S = 4;
  const DRAFT_PREFIX = "programmable-metrics-demo.draft:";
  const OLD_DRAFT_KEY = "programmable-metrics-demo.draft";
  const NO_EXPORT = "no-export";
  const SAMPLE = [
    "# ADP does not serve its export yet, so this is a sample program to try the editor with.",
    "policy post_enrich {",
    '  base "sha256:0000000000000000000000000000000000000000000000000000000000000000"',
    "",
    "  input.metrics -> drop_debug",
    "",
    '  node drop_debug = split(name starts_with "demo.debug.") {',
    "    true  -> drop",
    "    false -> emit dd_metrics_encode",
    "  }",
    "}",
    "",
  ].join("\n");

  const $ = (id) => document.getElementById(id);
  const ui = {
    source: $("source"), gutter: $("gutter"), highlights: $("highlights"), status: $("compile-status"),
    diagnostics: $("diagnostics"), publish: $("btn-publish"), format: $("btn-format"), revoke: $("btn-revoke"),
    outcome: $("publish-outcome"), confirm: $("confirm"), confirmButton: $("btn-confirm"),
    confirmHash: $("confirm-hash"), confirmRevision: $("confirm-revision"), confirmWarnings: $("confirm-warnings"),
    load: $("btn-load"), compile: $("btn-compile"), bindings: $("bindings"), bindingsRows: $("bindings-rows"),
    bindingsCount: $("bindings-count"), banner: $("banner"), intakes: $("intakes"), acks: $("acks"),
    ackSummary: $("ack-summary"), trafficButton: $("btn-traffic"), trafficRate: $("traffic-rate"),
    rateButton: $("btn-rate"), trafficStats: $("traffic-stats"), trafficTarget: $("traffic-target"),
    canary: $("canary"), nodes: $("nodes"), nodesHead: $("nodes-head"), nodesTitle: $("nodes-title"),
    nodesHint: $("nodes-hint"), planStats: $("plan-stats"), foot: $("foot"), toast: $("toast"),
    badgeAdp: $("badge-adp"), badgeSurface: $("badge-surface"), badgeProgram: $("badge-program"),
    diff: $("plan-diff"), diffHead: $("plan-diff-head"), diffNodes: $("plan-diff-nodes"),
    diffEdges: $("plan-diff-edges"), diffEdgesSummary: $("plan-diff-edges-summary"),
    diffEdgeList: $("plan-diff-edge-list"), confirmDiff: $("confirm-diff"),
  };
  const nodesTable = ui.nodes.closest("table");

  let state = null; // the last /api/state answer
  let firstSeenMs = Infinity; // when the page first had a state: families arriving soon after are not news
  let compiled = null; // { source, outcome, published } of the last compile; `published` once its approval is used
  // Where the editor's text came from ("empty", "sample", "default", "draft", or "user" once edited), and the
  // default program hash it belongs to, which keys its draft.
  const editor = { origin: "empty", base: null, loading: false, loadFailed: false };
  let toastTimer = null;

  // ---- Helpers --------------------------------------------------------------------------------------------------

  function el(tag, props, ...children) {
    const node = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) {
      if (key === "class") node.className = value;
      else if (key === "text") node.textContent = value;
      else if (key === "title") node.title = value;
      else node.setAttribute(key, value);
    }
    for (const child of children) {
      if (child !== null && child !== undefined) node.append(child);
    }
    return node;
  }

  function shortHash(hash) {
    if (!hash) return "—";
    const hex = String(hash).replace(/^sha256:/, "");
    return hex.length > 14 ? `sha256:${hex.slice(0, 8)}…${hex.slice(-4)}` : String(hash);
  }

  function tinyHash(hash) {
    if (!hash) return "—";
    const hex = String(hash).replace(/^sha256:/, "");
    return hex.length > 10 ? `${hex.slice(0, 8)}…` : hex;
  }

  function rate(value) {
    if (!Number.isFinite(value)) return "—";
    if (value >= 100) return Math.round(value).toLocaleString();
    if (value >= 10) return value.toFixed(1);
    return value.toFixed(value === 0 ? 0 : 2);
  }

  function count(value) {
    return Number.isFinite(value) ? Math.round(value).toLocaleString() : "—";
  }

  function plural(n, word) {
    return `${n} ${word}${n === 1 ? "" : "s"}`;
  }

  function ago(ms) {
    if (ms === null || ms === undefined || !state) return "never";
    const seconds = Math.max(0, (state.now_ms - ms) / 1000);
    return seconds < 60 ? `${seconds.toFixed(1)} s ago` : `${Math.round(seconds / 60)} min ago`;
  }

  function clock(ms) {
    return new Date(ms).toLocaleTimeString([], { hour12: false });
  }

  function toast(message, isError) {
    ui.toast.textContent = message;
    ui.toast.className = isError ? "toast err" : "toast";
    ui.toast.hidden = false;
    clearTimeout(toastTimer);
    toastTimer = setTimeout(() => { ui.toast.hidden = true; }, isError ? 8000 : 4000);
  }

  function sparkline(values) {
    const svgNs = "http://www.w3.org/2000/svg";
    const width = 60, height = 18;
    const svg = document.createElementNS(svgNs, "svg");
    svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
    svg.setAttribute("preserveAspectRatio", "none");
    svg.setAttribute("aria-hidden", "true");
    const max = Math.max(1, ...values);
    const step = values.length > 1 ? width / (values.length - 1) : width;
    const points = values.map((v, i) => {
      const y = height - 1 - (Math.max(0, v) / max) * (height - 3);
      return `${(i * step).toFixed(2)},${y.toFixed(2)}`;
    });
    const area = document.createElementNS(svgNs, "polygon");
    area.setAttribute("class", "spark-area");
    area.setAttribute("points", `0,${height} ${points.join(" ")} ${width},${height}`);
    const line = document.createElementNS(svgNs, "polyline");
    line.setAttribute("class", "spark-line");
    line.setAttribute("points", points.join(" "));
    svg.append(area, line);
    return svg;
  }

  // A moving average over SMOOTH_S seconds, of the seconds that have a whole window behind them: a partial window at
  // the start would draw a spike or a dip that was never there.
  function smooth(values) {
    const averaged = [];
    for (let i = SMOOTH_S - 1; i < values.length; i++) {
      averaged.push(values.slice(i - SMOOTH_S + 1, i + 1).reduce((a, b) => a + b, 0) / SMOOTH_S);
    }
    return averaged;
  }

  // Lets an identifier wrap after its dots or underscores rather than anywhere, in a narrow column.
  function breakable(text, separators) {
    const parts = [];
    let start = 0;
    for (let i = 0; i < text.length; i++) {
      if (separators.includes(text[i])) {
        parts.push(text.slice(start, i + 1), document.createElement("wbr"));
        start = i + 1;
      }
    }
    parts.push(text.slice(start));
    return parts;
  }

  // Calls the demo server. Throws if the server cannot be reached; otherwise returns the status and the JSON body.
  async function api(method, path, body) {
    const init = { method, headers: {} };
    if (body !== undefined) {
      init.headers["content-type"] = "application/json";
      init.body = JSON.stringify(body);
    }
    const response = await fetch(path, init);
    let data = null;
    try { data = await response.json(); } catch (_) { data = null; }
    return { ok: response.ok, status: response.status, data };
  }

  // Runs an action for a button: the button is disabled (and relabelled) while it runs, and a demo server that does
  // not answer is reported rather than leaving the page half-updated.
  async function busy(button, label, action) {
    if (button.dataset.busy) return;
    const text = button.textContent;
    button.dataset.busy = "1";
    button.disabled = true;
    button.classList.add("busy");
    if (label) button.textContent = label;
    try {
      await action();
    } catch (error) {
      toast(`The demo server did not answer: ${error.message}. Is programmable-metrics-demo still running?`, true);
    } finally {
      delete button.dataset.busy;
      button.classList.remove("busy");
      if (label) button.textContent = text;
      button.disabled = false;
      renderPublish();
    }
  }

  // ---- Drafts ---------------------------------------------------------------------------------------------------

  function draftKey(base) {
    return DRAFT_PREFIX + (base || NO_EXPORT);
  }

  function saveDraft() {
    try { localStorage.setItem(draftKey(editor.base), ui.source.value); } catch (_) { /* storage may be unavailable */ }
  }

  function loadDraft(base) {
    try { return localStorage.getItem(draftKey(base)); } catch (_) { return null; }
  }

  // Drops the drafts of other default programs: they were written against a program ADP no longer runs.
  function forgetOtherDrafts(base) {
    try {
      const keep = draftKey(base);
      const stale = [];
      for (let i = 0; i < localStorage.length; i++) {
        const key = localStorage.key(i);
        if (key && (key.startsWith(DRAFT_PREFIX) || key === OLD_DRAFT_KEY) && key !== keep) stale.push(key);
      }
      for (const key of stale) localStorage.removeItem(key);
    } catch (_) { /* storage may be unavailable */ }
  }

  // ---- Editor ---------------------------------------------------------------------------------------------------

  function lineSeverities() {
    const lines = new Map();
    if (!compiled || compiled.source !== ui.source.value) return lines;
    for (const d of compiled.outcome.diagnostics) {
      const kind = d.severity === "error" ? "err" : "warn";
      const last = Math.min(d.range.end_line, d.range.line + 60);
      for (let line = d.range.line; line <= last; line++) {
        if (lines.get(line) !== "err") lines.set(line, kind);
      }
    }
    return lines;
  }

  function renderEditor() {
    const lines = ui.source.value.split("\n").length;
    const severities = lineSeverities();
    const numbers = [];
    for (let n = 1; n <= lines; n++) {
      const kind = severities.get(n);
      numbers.push(kind ? `<span class="${kind}">${n}</span>` : String(n));
    }
    ui.gutter.innerHTML = numbers.join("\n");
    const last = Math.max(0, ...severities.keys());
    const rows = [];
    for (let n = 1; n <= last; n++) rows.push(`<div class="${severities.get(n) || ""}"></div>`);
    ui.highlights.innerHTML = rows.join("");
    syncScroll();
  }

  function syncScroll() {
    ui.gutter.scrollTop = ui.source.scrollTop;
    ui.highlights.style.transform = `translateY(${-ui.source.scrollTop}px)`;
  }

  // Converts a line and a column (both from 1, the column in Unicode scalar values) to an index into the text.
  function indexAt(text, line, column) {
    let index = 0;
    for (let l = 1; l < line; l++) {
      const newline = text.indexOf("\n", index);
      if (newline < 0) return text.length;
      index = newline + 1;
    }
    for (let c = 1; c < column && index < text.length && text[index] !== "\n"; c++) {
      index += text.codePointAt(index) > 0xffff ? 2 : 1;
    }
    return index;
  }

  function reveal(range) {
    const text = ui.source.value;
    const start = indexAt(text, range.line, range.column);
    const end = Math.max(start, indexAt(text, range.end_line, range.end_column));
    ui.source.focus();
    ui.source.setSelectionRange(start, end);
    const lineHeight = parseFloat(getComputedStyle(ui.source).lineHeight) || 20;
    ui.source.scrollTop = Math.max(0, (range.line - 4) * lineHeight);
    syncScroll();
  }

  function refreshEditor() {
    renderEditor();
    renderCompileStatus();
    renderPreview();
    renderPublish();
  }

  // Only what the user types is a draft; text the page puts in the editor is not.
  function onUserEdit() {
    editor.origin = "user";
    saveDraft();
    refreshEditor();
  }

  ui.source.addEventListener("input", onUserEdit);
  ui.source.addEventListener("scroll", syncScroll);
  ui.source.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      busy(ui.compile, "Compiling…", compile);
    } else if (event.key === "Tab" && !event.shiftKey && !event.altKey) {
      event.preventDefault();
      ui.source.setRangeText("  ", ui.source.selectionStart, ui.source.selectionEnd, "end");
      onUserEdit();
    }
  });

  function setSource(text, origin, base) {
    ui.source.value = text;
    ui.source.scrollTop = 0;
    editor.origin = origin;
    editor.base = base;
    refreshEditor();
  }

  function setStatus(tone, text) {
    ui.status.className = tone ? `compile-status ${tone}` : "compile-status";
    ui.status.textContent = text;
  }

  // ---- Compile, publish, revoke ---------------------------------------------------------------------------------

  async function compile() {
    const source = ui.source.value;
    setStatus("", "Compiling…");
    try {
      const { ok, data } = await api("POST", "/api/compile", { source });
      if (!ok || !data) throw new Error("the demo server refused the compile request");
      compiled = { source, outcome: data, published: null };
    } catch (error) {
      compiled = null;
      setStatus("err", `Compile failed: ${error.message}.`);
      renderDiagnostics();
      renderPreview();
      renderPublish();
      return;
    }
    renderEditor();
    renderCompileStatus();
    renderDiagnostics();
    renderPreview();
    renderPublish();
  }

  function renderCompileStatus() {
    if (!compiled) return;
    const outcome = compiled.outcome;
    ui.status.replaceChildren();
    if (compiled.source !== ui.source.value) {
      setStatus("", "Edited since the last compile. Compile again to publish.");
      return;
    }
    if (!outcome.ok) {
      setStatus("err", `${plural(outcome.errors, "error")}: no artifact.`);
      return;
    }
    // Say what ran: composition and validation as ADP runs them, or, when ADP does not serve its default program,
    // structural checks only, which leave the verdict to ADP.
    const checked = outcome.checked_against;
    const found = [];
    if (outcome.errors) found.push(plural(outcome.errors, "error"));
    if (outcome.warnings) found.push(plural(outcome.warnings, "warning"));
    let tone = "", verdict;
    if (!checked.available) {
      tone = "warn";
      verdict = "not checked: ADP's export is not available";
    } else if (checked.composed) {
      if (outcome.errors) {
        tone = "err";
        verdict = `composed as ADP will: ${found.join(" and ")}, so ADP will reject it`;
      } else if (outcome.warnings) {
        tone = "warn";
        verdict = `composed and validated as ADP will: ${found.join(" and ")}`;
      } else {
        tone = "ok";
        verdict = "composed into ADP's default program and validated as ADP will: no findings";
      }
    } else if (outcome.errors) {
      tone = "err";
      verdict = `structural checks found ${found.join(" and ")}: ADP will reject it`;
    } else if (outcome.warnings) {
      tone = "warn";
      verdict = `structural checks found ${found.join(" and ")}`;
    } else {
      verdict = "structural checks passed";
    }
    const notes = [];
    if (outcome.restored_fingerprints.length) {
      notes.push(`${plural(outcome.restored_fingerprints.length, "output fingerprint")} from ADP's export.`);
    }
    if (checked.reason) notes.push(checked.reason);
    ui.status.className = tone ? `compile-status ${tone}` : "compile-status";
    ui.status.append(
      "Compiled ",
      el("code", { text: shortHash(outcome.artifact_hash), title: outcome.artifact_hash }),
      ` · ${verdict}.`,
      el("span", { class: "note", text: notes.join(" ") }),
    );
  }

  function renderDiagnostics() {
    ui.diagnostics.replaceChildren();
    if (!compiled) return;
    const origins = {
      syntax: "syntax", document: "receipt", check: "check", composition: "composition", validation: "validation",
    };
    for (const d of compiled.outcome.diagnostics) {
      // Where the finding comes from: the validation rule, or the step that made it.
      const tag = el("span", { class: "tag", text: d.rule || origins[d.origin] || d.origin });
      const message = el("span", { class: "message clamp", title: d.message }, tag, d.message);
      const item = el(
        "li",
        { class: d.severity, title: `${d.origin} ${d.severity}` },
        el("span", { class: "where", text: `${d.range.line}:${d.range.column}` }),
        message,
      );
      // A click selects the span, and shows the whole message.
      item.addEventListener("click", () => {
        message.classList.add("expanded");
        reveal(d.range);
      });
      ui.diagnostics.append(item);
    }
  }

  // Whether a diff entry only changes a node's provenance: the artifact now authorizes what native configuration did.
  function provenanceOnly(node) {
    return node.change === "changed" && node.aspects.length === 1 && node.aspects[0] === "provenance";
  }

  function diffBase(against) {
    if (against.kind !== "active") return "the default program (ADP's active plan was not available)";
    const program = against.artifact_revision ? `artifact rev ${against.artifact_revision}` : "default program";
    return `the active plan (gen ${against.generation ?? "?"}, ${program})`;
  }

  // Counts a preview's changes in a few words, for the diff's header and the approval panel.
  function diffCounts(preview) {
    const structural = preview.nodes.filter((n) => !provenanceOnly(n));
    const parts = [];
    const added = structural.filter((n) => n.change === "added").length;
    const changed = structural.filter((n) => n.change === "changed").length;
    const removed = structural.filter((n) => n.change === "removed").length;
    if (added) parts.push(`${added} added`);
    if (changed) parts.push(`${changed} changed`);
    if (removed) parts.push(`${removed} removed`);
    const authority = preview.nodes.length - structural.length;
    if (authority) parts.push(`${authority} re-authorized`);
    const outputs = preview.outputs.length;
    if (outputs) parts.push(plural(outputs, "output"));
    return parts.length ? parts.join(" · ") : "edges only";
  }

  // The plan diff of the last compile, next to Publish: what installing the artifact changes in ADP's active plan.
  function renderPreview() {
    const fresh = freshCompile();
    ui.diffNodes.replaceChildren();
    ui.diffEdgeList.replaceChildren();
    ui.diffEdges.hidden = true;
    if (!fresh) {
      // No verdict of an earlier compile may describe what the editor holds now.
      ui.diff.hidden = true;
      ui.diff.className = "plan-diff";
      ui.diffHead.replaceChildren();
      return;
    }
    const preview = fresh.outcome.preview;
    ui.diff.hidden = false;
    if (!preview.available) {
      ui.diff.className = "plan-diff none";
      ui.diffHead.replaceChildren(el("strong", { text: "No plan diff. " }),
        preview.reason.replace(/^No plan diff: /, ""));
      return;
    }
    const base = el("span", { class: "against", text: ` against ${diffBase(preview.against)}` });
    const stale = preview.stale_base ? " · stale base" : "";
    // Whether ADP has moved on since the compile: it runs this plan now, or another than the one diffed against.
    const latest = state && latestStatus(state);
    const active = latest && latest.status.active_plan_hash;
    let since = null;
    if (active && active === preview.plan_hash && active !== preview.against.plan_hash) {
      const text = ` · ✓ ADP runs it now (gen ${latest.status.active_generation})`;
      since = el("span", { class: "since ok", text });
    } else if (active && active !== preview.against.plan_hash && active !== preview.plan_hash) {
      const text = " · ADP's active plan changed since: compile again to diff against it";
      since = el("span", { class: "since", text });
    }
    if (preview.unchanged) {
      ui.diff.className = "plan-diff unchanged";
      ui.diffHead.replaceChildren(el("strong", { text: "✓ No change" }), base,
        `: ADP keeps running it${stale}.`, since || "");
      return;
    }
    ui.diff.className = preview.valid ? "plan-diff" : "plan-diff invalid";
    const verdict = preview.valid ? "" : " · does not validate: ADP will reject it";
    ui.diffHead.replaceChildren(el("strong", { text: "Plan diff" }), base, `: ${diffCounts(preview)}${stale}${verdict}`,
      since || "");

    const signs = { added: "+", removed: "−", changed: "~" };
    for (const node of preview.nodes.filter((n) => !provenanceOnly(n))) {
      let what;
      if (node.change === "added") what = `${node.op} · ${node.provenance || ""}`;
      else if (node.change === "removed") what = node.op;
      else what = `${node.op}: ${node.aspects.join(", ")}`;
      const name = node.artifact_node && node.artifact_node !== node.node
        ? `${node.node} (${node.artifact_node})` : node.node;
      const item = el("li", { class: node.change },
        el("span", { class: "sign", text: signs[node.change] || "?" }),
        el("span", { class: "id", text: name }),
        el("span", { class: "what", text: what }));
      if (node.range) {
        item.dataset.line = String(node.range.line);
        item.title = `Line ${node.range.line}: click to select it in the editor`;
        item.addEventListener("click", () => reveal(node.range));
      }
      ui.diffNodes.append(item);
    }
    const reauthorized = preview.nodes.filter(provenanceOnly);
    if (reauthorized.length) {
      const ids = reauthorized.map((n) => n.node);
      const item = el("li", { class: "changed authority" },
        el("span", { class: "sign", text: "~" }),
        el("span", { class: "id", text: ids.join(", ") }),
        el("span", { class: "what", text: `now authorized by ${reauthorized[0].provenance || "the artifact"}` }));
      item.title = "Unchanged nodes whose loss the artifact authorizes now, since what reaches them changed";
      ui.diffNodes.append(item);
    }
    for (const output of preview.outputs) {
      ui.diffNodes.append(el("li", { class: "changed" },
        el("span", { class: "sign", text: "~" }),
        el("span", { class: "id", text: output.output }),
        el("span", { class: "what", text: `output ${output.change}` })));
    }
    const edges = [
      ...preview.edges_added.map((edge) => `+ ${edge}`),
      ...preview.edges_removed.map((edge) => `− ${edge}`),
    ];
    if (edges.length) {
      ui.diffEdges.hidden = false;
      ui.diffEdgesSummary.textContent = `${plural(preview.edges_added.length, "edge")} added, `
        + `${preview.edges_removed.length} removed`;
      ui.diffEdgeList.replaceChildren(...edges.map((text) => el("li", { text })));
    }
  }

  // Returns ADP's latest status, with when it arrived: the newer of the latest poll's and the privileged route's.
  function latestStatus(s) {
    const poll = s.channel.last_poll;
    const polled = poll && !poll.error && poll.status ? { status: poll.status, at: poll.received_ms } : null;
    const control = s.control.status && s.control.status_received_ms
      ? { status: s.control.status, at: s.control.status_received_ms } : null;
    if (polled && control) return polled.at >= control.at ? polled : control;
    return polled || control;
  }

  // Returns ADP's latest acknowledgments, from the newer of the latest poll and the privileged status route.
  function latestAcks(s) {
    const poll = s.channel.last_poll;
    const polled = poll && !poll.error ? { states: poll.config_states, at: poll.received_ms } : null;
    const control = s.control.status && s.control.status_received_ms
      ? { states: s.control.status.config_states, at: s.control.status_received_ms } : null;
    const newest = polled && control ? (polled.at >= control.at ? polled : control) : polled || control;
    return newest ? newest.states : [];
  }

  function staleAfterMs(s) {
    return Math.max(3000, 3 * s.poll_interval_ms);
  }

  function activeArtifactHash() {
    const latest = state && latestStatus(state);
    return latest && latest.status.active_artifact ? latest.status.active_artifact.artifact_hash : null;
  }

  function freshCompile() {
    return compiled && compiled.outcome.ok && compiled.source === ui.source.value ? compiled : null;
  }

  function publishable() {
    const fresh = freshCompile();
    return Boolean(fresh && !fresh.published && fresh.outcome.artifact_hash !== activeArtifactHash());
  }

  function renderPublish() {
    const fresh = freshCompile();
    const hash = fresh && fresh.outcome.artifact_hash;
    if (!ui.publish.dataset.busy) {
      ui.publish.disabled = !publishable();
      if (!fresh) {
        ui.publish.textContent = "Publish";
        ui.publish.title = "Compile the program first";
      } else if (fresh.published) {
        ui.publish.textContent = `Published as revision ${fresh.published.revision}`;
        ui.publish.title = "This compile's approval was used. Compile again to publish it as another revision.";
      } else if (hash === activeArtifactHash()) {
        ui.publish.textContent = "Already active";
        ui.publish.title = `ADP already runs ${hash}`;
      } else {
        ui.publish.textContent = `Publish ${shortHash(hash)}`;
        ui.publish.title = `Approve and publish ${hash}`;
      }
    }
    const canonical = fresh && fresh.outcome.canonical_source;
    ui.format.hidden = !canonical || canonical === ui.source.value;
    if (ui.publish.disabled && !ui.confirmButton.dataset.busy) ui.confirm.hidden = true;
    if (!ui.revoke.dataset.busy) ui.revoke.disabled = !(state && state.channel.current);
  }

  ui.format.addEventListener("click", () => {
    const fresh = freshCompile();
    if (!fresh) return;
    // The canonical form parses to the same artifact, so the compile stays valid, and the text is the user's.
    fresh.source = fresh.outcome.canonical_source;
    setSource(fresh.outcome.canonical_source, "user", editor.base);
    saveDraft();
    renderDiagnostics();
  });

  ui.publish.addEventListener("click", () => {
    if (!publishable()) return;
    const outcome = compiled.outcome;
    ui.confirmHash.textContent = outcome.artifact_hash;
    ui.confirmRevision.textContent = state ? String(state.channel.next_revision) : "next";
    const preview = outcome.preview;
    let diffText = null;
    if (preview.available && preview.unchanged) {
      diffText = `It composes to ${diffBase(preview.against)} itself: ADP acknowledges it without a new generation.`;
    } else if (preview.available) {
      diffText = `Installing it changes ${diffBase(preview.against)}: ${diffCounts(preview)}.`;
    }
    ui.confirmDiff.textContent = diffText || "";
    ui.confirmDiff.hidden = !diffText;
    const warnings = [];
    if (outcome.errors) {
      const first = outcome.diagnostics.find((d) => d.severity === "error");
      const how = outcome.checked_against.composed ? "Composed as ADP will" : "The structural checks";
      warnings.push(`${how}, it has ${plural(outcome.errors, "error")}: ADP will reject it and keep running its `
        + `active plan. ${first ? first.message : ""}`);
    }
    if (!outcome.checked_against.available) {
      warnings.push("Not checked against ADP's catalog: ADP's export is not available.");
    }
    // The approval names the active plan the compile diffed against: if ADP runs another one now, the diff above is
    // not what installing it changes, and ADP flags the review as stale.
    const latest = state && latestStatus(state);
    const active = latest && latest.status.active_plan_hash;
    if (preview.available && preview.against.kind === "active" && active && active !== preview.against.plan_hash) {
      const generation = latest.status.active_generation ?? "?";
      warnings.push(`ADP's active plan changed since the compile (it runs gen ${generation} now): the diff above is `
        + "against the plan it ran then, and ADP will flag this review as stale. Compile again to review what "
        + "installing it changes now.");
    }
    const lastPoll = state && state.channel.last_poll;
    if (state && !state.channel.polls_total) {
      warnings.push("ADP has never polled the artifact channel stub: the artifact waits in the snapshot until it "
        + "does.");
    } else if (lastPoll && state.now_ms - lastPoll.received_ms > staleAfterMs(state)) {
      warnings.push(`ADP last polled the stub ${ago(lastPoll.received_ms)}.`);
    }
    const warning = (text) => el("li", {}, el("div", { class: "clamp", text, title: text }));
    ui.confirmWarnings.replaceChildren(...warnings.map(warning));
    ui.confirmWarnings.hidden = warnings.length === 0;
    ui.confirm.hidden = false;
    ui.confirm.scrollIntoView({ block: "nearest" });
  });

  $("btn-cancel").addEventListener("click", () => { ui.confirm.hidden = true; });

  ui.confirmButton.addEventListener("click", () => busy(ui.confirmButton, "Publishing…", async () => {
    try {
      if (!publishable()) return;
      const approved = compiled;
      const { ok, data } = await api("POST", "/api/publish", { artifact_hash: approved.outcome.artifact_hash });
      if (ok) {
        approved.published = { revision: data.revision, hash: data.artifact_hash };
        toast(`Published revision ${data.revision} (${shortHash(data.artifact_hash)}).`);
      } else {
        toast(`Publish refused: ${(data && data.error) || "unknown error"}`, true);
      }
    } finally {
      ui.confirm.hidden = true;
      poll();
    }
  }));

  ui.revoke.addEventListener("click", () => busy(ui.revoke, "Revoking…", async () => {
    const question = "Revoke the published artifact? The stub serves an empty snapshot and ADP installs the default "
      + "program.";
    if (!window.confirm(question)) return;
    const { ok, data } = await api("POST", "/api/revoke");
    if (ok && data && data.revoked) toast("Revoked: the snapshot is empty, and ADP returns to the default program.");
    else if (ok) toast((data && data.message) || "Nothing to revoke.");
    else toast(`Revoke failed: ${(data && data.error) || "unknown error"}`, true);
    poll();
  }));

  // Loads the default program from ADP's export. On the first load, and when ADP starts serving its export, a draft
  // of the same default program comes back instead; without an export, a sample program stands in.
  async function loadDefault(initial) {
    if (editor.loading) return;
    editor.loading = true;
    try {
      let data;
      try {
        data = (await api("GET", "/api/default-program")).data;
      } catch (error) {
        data = { available: false, message: `The demo server did not answer: ${error.message}.` };
      }
      if (data && data.available) {
        const base = data.program_hash;
        editor.loadFailed = false;
        if (initial) {
          const draft = loadDraft(base);
          forgetOtherDrafts(base);
          if (draft && draft.trim() && draft !== data.source) {
            setSource(draft, "draft", base);
            setStatus("", "Restored your draft of this default program. Load the default program to start over.");
            return;
          }
        } else if (["user", "draft"].includes(editor.origin) && ui.source.value !== data.source
          && !window.confirm("Replace the editor's text with the default program?")) {
          return;
        }
        setSource(data.source, "default", base);
        setStatus("", `Loaded the default program (${shortHash(base)}). Edit it, then compile.`);
        return;
      }
      const message = (data && data.message) || "The demo server did not answer.";
      editor.loadFailed = true;
      if (initial && editor.origin === "empty") {
        const draft = loadDraft(null);
        if (draft && draft.trim()) {
          setSource(draft, "draft", null);
          setStatus("warn", `${message} Restored your draft.`);
          return;
        }
        setSource(SAMPLE, "sample", null);
        setStatus("warn", `${message} A sample program is in the editor instead.`);
        return;
      }
      setStatus("warn", message);
    } finally {
      editor.loading = false;
    }
  }

  // Follows the export: the editor loads the default program once there is one (and again when it changes), unless the
  // user has written something. Attempts are at least AUTOLOAD_RETRY_MS apart, since each one asks ADP.
  const AUTOLOAD_RETRY_MS = 5000;
  let lastAutoload = 0;
  function followExport(s) {
    const exported = s.control.export_program_hash;
    if (!exported || editor.loading) return;
    if (["empty", "sample"].includes(editor.origin) || (editor.origin === "default" && editor.base !== exported)) {
      if (Date.now() - lastAutoload < AUTOLOAD_RETRY_MS) return;
      lastAutoload = Date.now();
      loadDefault(true);
    } else if (editor.loadFailed && editor.base !== exported) {
      editor.loadFailed = false;
      setStatus("", "ADP now serves its export: Load the default program to start from it.");
    }
  }

  ui.load.title = "Replaces the editor's text with ADP's default program. An artifact published from it replaces the "
    + "one in effect, so it also undoes that one's changes: keep editing the current text to add a change, or revoke "
    + "first to show one change at a time.";
  ui.load.addEventListener("click", () => busy(ui.load, "Loading…", () => loadDefault(false)));
  ui.compile.addEventListener("click", () => busy(ui.compile, "Compiling…", compile));

  // ---- Traffic --------------------------------------------------------------------------------------------------

  ui.trafficButton.addEventListener("click", () => busy(ui.trafficButton, null, async () => {
    if (!state) return;
    await api("POST", "/api/traffic", { running: !state.traffic.running });
    await poll();
  }));

  ui.rateButton.addEventListener("click", () => busy(ui.rateButton, null, async () => {
    const value = Number(ui.trafficRate.value);
    if (!Number.isFinite(value) || value <= 0) {
      toast("Enter a rate above zero.", true);
      return;
    }
    await api("POST", "/api/traffic", { rate: value });
    await poll();
  }));

  // ---- State rendering ------------------------------------------------------------------------------------------

  function setBadge(node, text, kind, title) {
    node.textContent = text;
    node.className = `badge ${kind || ""}`.trim();
    node.title = title || "";
  }

  function renderBadges(s) {
    const phase = s.adp.phase;
    const telemetryOk = s.telemetry.state === "ok";
    if (phase === "ready") setBadge(ui.badgeAdp, "ADP ready", "ok", `pid ${s.adp.pid}`);
    else if (phase === "starting") setBadge(ui.badgeAdp, "ADP starting", "warn");
    else if (phase === "exited") setBadge(ui.badgeAdp, `ADP ${s.adp.exit || "exited"}`, "err");
    else if (telemetryOk) setBadge(ui.badgeAdp, "ADP (external) up", "ok");
    else setBadge(ui.badgeAdp, "ADP (external) not answering", "warn");

    const surface = s.control.status_state;
    if (surface === "available") setBadge(ui.badgeSurface, "Control surface", "ok");
    else if (surface === "not_exposed") {
      setBadge(ui.badgeSurface, "Control surface not exposed", "warn", s.control.message);
    } else if (surface === "failed") {
      setBadge(ui.badgeSurface, "Control surface unreachable", "err", s.control.message);
    }
    else setBadge(ui.badgeSurface, "Control surface", "");

    const latest = latestStatus(s);
    const node = ui.badgeProgram;
    if (!latest) {
      node.textContent = "Default program";
      node.className = "badge badge-program";
      node.title = "ADP has not reported its status: with no artifact channel, it runs the default program.";
      return;
    }
    const status = latest.status;
    const generation = status.active_generation ?? "?";
    let text, kind, title;
    if (status.active_artifact) {
      text = `Artifact rev ${status.active_artifact.revision ?? "?"} · gen ${generation}`;
      kind = "artifact";
      title = `Active artifact ${status.active_artifact.artifact_hash || ""}`;
      // Flags that ADP installed the artifact although it changed since the approval: a stale base or review.
      const stale = [status.stale_base ? "stale base" : null, status.stale_review ? "stale review" : null]
        .filter(Boolean);
      if (stale.length) {
        text = `${text} · ${stale.join(" · ")}`;
        title = `${title}. ${stale.includes("stale review") ? "It was approved against another active plan than the "
          + "one ADP installed it over. " : ""}${stale.includes("stale base") ? "Its base is not ADP's current "
          + "default program. " : ""}ADP runs it anyway.`;
      }
    } else {
      text = `Default program · gen ${generation}`;
      kind = "ok";
      title = `Default program ${status.default_program_hash || ""}`;
    }
    const age = s.now_ms - latest.at;
    if (age > staleAfterMs(s)) {
      text = `Last known: ${text}`;
      kind = "stale";
      title = `ADP has not reported its status for ${Math.round(age / 1000)} s. ${title}`;
    }
    node.textContent = text;
    node.className = `badge badge-program ${kind}`;
    node.title = title;
  }

  let bannerDetails = null; // the not-exposed explanation, kept so that it stays open once opened

  function renderBanner(s) {
    ui.banner.replaceChildren();
    ui.banner.className = "banner";
    if (s.adp.phase === "exited") {
      ui.banner.classList.add("err");
      ui.banner.append(
        el("strong", { text: `ADP ${s.adp.exit || "exited"}.` }),
        ` Its output is in ${s.adp.console_log_path}; restart the demo to start it again.`,
      );
    } else if (s.control.message && s.control.detail) {
      // One line, with what it means for the demo a click away, so that it does not crowd the editor.
      if (!bannerDetails || bannerDetails.dataset.message !== s.control.message) {
        const summary = el("summary", {}, el("strong", { text: s.control.message }),
          el("span", { class: "more", text: "What this means" }));
        bannerDetails = el("details", { "data-message": s.control.message }, summary,
          el("p", { text: s.control.detail }));
      }
      ui.banner.append(bannerDetails);
    } else if (s.control.message) {
      ui.banner.append(el("strong", { text: s.control.message }));
    }
    ui.banner.hidden = !ui.banner.childNodes.length;
  }

  function renderBindings(s) {
    const bindings = s.legend ? s.legend.bindings : [];
    ui.bindings.hidden = bindings.length === 0;
    ui.bindingsCount.textContent = bindings.length ? `(${bindings.length}, from ADP's export)` : "";
    ui.bindingsRows.replaceChildren(...bindings.map((binding) => el("tr", {},
      el("td", { text: binding.name }),
      el("td", {
        class: binding.live ? "live" : "",
        text: binding.value === null || binding.value === undefined ? "—" : JSON.stringify(binding.value),
      }))));
  }

  // Intake columns are kept between renders, keyed by family, so that fading is a smooth transition.
  const intakeColumns = new Map();

  function intakeColumn(intake) {
    let column = intakeColumns.get(intake.name);
    if (!column) {
      const name = el("div", { class: "intake-name", text: intake.label });
      const outputs = el("div", { class: "intake-outputs" });
      const alerts = el("div", {});
      const meta = el("div", { class: "intake-meta" });
      const list = el("ul", { class: "families" });
      const empty = el("div", { class: "empty", text: "Nothing in the last 5 s." });
      const head = el("div", { class: "intake-head" }, name, outputs, alerts, meta);
      const root = el("div", { class: "intake", "data-name": intake.name }, head, list, empty);
      column = { root, outputs, alerts, meta, list, empty, families: new Map() };
      intakeColumns.set(intake.name, column);
      ui.intakes.append(root);
    }
    return column;
  }

  function renderIntakes(s) {
    const outputs = s.legend ? s.legend.outputs : [];
    const nowS = Math.floor(s.now_ms / 1000);
    for (const intake of s.intakes) {
      const column = intakeColumn(intake);
      // The outputs of the default program that feed this endpoint, from ADP's export.
      const feeding = outputs.filter((output) => output.intake === intake.name);
      column.outputs.replaceChildren(...feeding.map((output) => el("div", {}, "← ", ...breakable(output.id, "_"))));
      column.outputs.title = feeding.map((output) => `${output.id}: ${output.label || "no label"}`).join("\n");

      // Problems first, each on its own line, so that they are never cut off.
      const alerts = [];
      for (const key of intake.api_keys.filter((k) => !k.expected && k.key)) {
        alerts.push(el("div", { class: "intake-alert", text: `unexpected API key ${key.key} (${key.requests} req)` }));
      }
      if (intake.refused) {
        const last = intake.last_refusal;
        alerts.push(el("div", {
          class: "intake-alert",
          text: `${intake.refused} refused (last: ${last.status})`,
          title: `${last.path}: ${last.error || ""}`,
        }));
      }
      column.alerts.replaceChildren(...alerts);
      const last = intake.last_request_ms ? ago(intake.last_request_ms) : "no requests yet";
      column.meta.textContent = `:${intake.port} · ${intake.requests_total} req · ${last}`;
      column.meta.title = `API key ${intake.expected_api_key}`;

      const visible = intake.families.filter((f) => f.age_ms <= LIST_WINDOW_MS);
      const seen = new Set();
      for (const family of visible) {
        seen.add(family.family);
        let row = column.families.get(family.family);
        if (!row) {
          const nameNode = el("div", { class: "family-name" });
          const line = el("div", { class: "family-line" });
          row = { root: el("li", { class: "family" }, nameNode, line), nameNode, line };
          column.families.set(family.family, row);
        }
        const complete = family.per_second.slice(0, -1); // the current second is still filling
        // Average only over the seconds the family has been arriving here, so a newly routed family shows its rate
        // at once instead of climbing to it.
        const seconds = Math.min(RATE_MAX_S, Math.max(RATE_MIN_S, nowS - Math.floor(family.active_since_ms / 1000)));
        const recent = complete.slice(-seconds);
        const perSecond = recent.reduce((a, b) => a + b, 0) / seconds;
        // Allow names to wrap after their dots rather than be cut off in a narrow column.
        row.nameNode.replaceChildren(...breakable(family.family, "."));
        if (family.names > 1) row.nameNode.append(el("span", { class: "count", text: ` ×${family.names}` }));
        // A family that began arriving here a moment ago, well after the demo started, is what a change just routed
        // here: it is marked for a few seconds.
        const arrived = s.now_ms - family.active_since_ms < ARRIVED_MS && family.active_since_ms - firstSeenMs > 15000;
        if (arrived) row.nameNode.append(el("span", { class: "new", text: "new" }));
        row.root.classList.toggle("arrived", arrived);
        const more = family.names > 1 ? ` and ${family.names - 1} more` : "";
        row.nameNode.title = `${family.example}${more} · ${family.kinds.join(", ")} · `
          + `${count(family.points_total)} points`;
        const unit = family.unit === "points" ? "pts/s" : "ev/s";
        const unitNode = el("span", { class: "unit", text: ` ${unit}` });
        const rateNode = el("span", { class: "family-rate" }, rate(perSecond), unitNode);
        rateNode.title = family.unit === "points"
          ? "Points per second: a gauge arrives once per context per flush."
          : "Events per second: counted values of counts and rates, samples of distributions.";
        row.line.replaceChildren(sparkline(smooth(complete)), rateNode);
        row.root.classList.toggle("fading", family.age_ms > FADE_AFTER_MS);
      }
      for (const [key, row] of column.families) {
        if (!seen.has(key)) {
          row.root.remove();
          column.families.delete(key);
        }
      }
      // Keep rows in family order.
      for (const family of visible) column.list.append(column.families.get(family.family).root);
      column.empty.hidden = visible.length > 0;
    }
  }

  function renderOutcome(s) {
    const current = s.channel.current;
    const node = ui.outcome;
    node.replaceChildren();
    if (!current) {
      const lastChange = s.channel.history[0];
      node.hidden = !(lastChange && lastChange.action === "revoke");
      node.className = "publish-outcome";
      node.append("Revoked: the stub serves an empty snapshot, so ADP runs the default program.");
      return;
    }
    const ack = latestAcks(s).find((cs) => cs.revision === current.revision && cs.id === current.id);
    const latest = latestStatus(s);
    const lastPoll = s.channel.last_poll;
    let tone = "pending", text;
    if (!s.channel.polls_total) {
      text = "waiting: ADP has never polled the artifact channel stub";
    } else if (!lastPoll || lastPoll.received_ms < current.published_ms) {
      text = "waiting for ADP's next poll";
    } else if (ack && ack.state === "active") {
      tone = "ok";
      text = `active in generation ${ack.generation ?? "?"}`;
    } else if (ack && ["rejected", "failed"].includes(ack.state)) {
      tone = "err";
      text = `${ack.state}: ${ack.error || "no reason given"}`;
    } else if (ack) {
      text = ack.state || "acknowledged";
    } else if (latest && latest.status.active_artifact
      && latest.status.active_artifact.artifact_hash === current.artifact_hash) {
      tone = "ok";
      text = "active (the artifact ADP already ran)";
    } else {
      text = "delivered; ADP has not acknowledged it yet";
    }
    node.className = `publish-outcome ${tone}`;
    const revision = `Revision ${current.revision} (${tinyHash(current.artifact_hash)}): `;
    // A rejection names the paths it is about, which can take many lines: two are shown until a click shows the rest.
    const expanded = expandedOutcome === current.revision;
    const body = el("div", { class: `clamp two clamp-toggle${expanded ? " expanded" : ""}` },
      revision, el("span", { class: "state", text }));
    body.title = expanded ? current.artifact_hash : `${text}\n\nClick to show the whole message.`;
    body.addEventListener("click", () => {
      expandedOutcome = expandedOutcome === current.revision ? null : current.revision;
      renderOutcome(s);
    });
    node.append(body);
    node.hidden = false;
  }

  let expandedOutcome = null; // the revision whose outcome is shown whole

  const expandedDetails = new Set(); // the acknowledgment details shown whole, by entry

  function renderAcks(s) {
    const timeline = s.channel.timeline;
    const rows = [];
    for (const [index, entry] of timeline.entries()) {
      // Each entry is a run of polls that reported the same thing: when it started, and how many polls it has.
      const polls = entry.polls > 1 ? el("span", { class: "polls", text: ` ×${entry.polls}` }) : null;
      const when = [clock(entry.first_ms), polls];
      const status = entry.status || {};
      const older = timeline[index + 1];
      const restarted = older && older.client_id !== entry.client_id ? "new ADP process" : null;
      const unreadable = entry.notes.length ? `unreadable: ${entry.notes.join("; ")}` : null;
      if (entry.error) {
        rows.push([when, "—", null, "refused", "—", entry.error, "err"]);
        continue;
      }
      const notes = [restarted, status.degraded, status.stale_base ? "stale base" : null,
        status.stale_review ? "stale review" : null, unreadable]
        .filter(Boolean).join(" · ");
      const tone = unreadable ? "warn" : "";
      const states = entry.config_states.length ? entry.config_states : [null];
      for (const cs of states) {
        if (!cs) {
          const generation = status.active_generation ?? "—";
          rows.push([when, "—", null, "default", generation, notes || "default program", tone]);
        } else {
          const generation = cs.generation ?? "—";
          const detail = [cs.error, notes].filter(Boolean).join(" · ");
          rows.push([when, cs.revision ?? "—", cs.artifact_hash, cs.state || "?", generation, detail,
            cs.error ? "err" : tone]);
        }
      }
      if (rows.length >= 14) break;
    }
    ui.acks.replaceChildren();
    if (!rows.length) {
      const text = "ADP has not polled the artifact channel stub yet.";
      ui.acks.append(el("tr", {}, el("td", { colspan: "5", class: "detail", text })));
    }
    // The detail, when there is one, gets a row of its own under its entry, so that an error is never cut off.
    for (const [when, revision, hash, stateName, generation, detail, tone] of rows) {
      ui.acks.append(el(
        "tr", { class: detail ? "has-detail" : "" },
        el("td", {}, ...when),
        el("td", { class: "num", text: String(revision) }),
        el("td", { class: "mono", text: tinyHash(hash), title: hash || "" }),
        el("td", {}, el("span", { class: `pill ${stateName}`, text: stateName })),
        el("td", { class: "num", text: String(generation) }),
      ));
      if (detail) {
        const key = `${when[0]}:${revision}:${detail.length}`;
        const text = el("div", {
          class: `clamp clamp-toggle${expandedDetails.has(key) ? " expanded" : ""}`,
          text: detail,
          title: "Click to show or hide the whole message",
        });
        text.addEventListener("click", () => {
          if (!expandedDetails.delete(key)) expandedDetails.add(key);
          text.classList.toggle("expanded");
        });
        ui.acks.append(el("tr", { class: "detail-row" },
          el("td", { colspan: "5", class: tone ? `detail ${tone}` : "detail" }, text)));
      }
    }
    const current = s.channel.current;
    const configs = s.channel.configs || [];
    const parts = [`${s.channel.polls_total} polls`];
    if (s.channel.last_poll) parts.push(`last ${ago(s.channel.last_poll.received_ms)}`);
    if (configs.length > 1 || (configs.length === 1 && !current)) {
      // One config per scope: name each scope's revision.
      parts.push(`serving ${configs.map((config) => `rev ${config.revision} (${config.scope})`).join(", ")}`);
    } else {
      parts.push(current ? `serving rev ${current.revision}` : "serving an empty snapshot");
    }
    ui.ackSummary.textContent = parts.join(" · ");
  }

  function renderTraffic(s) {
    const t = s.traffic;
    ui.trafficButton.textContent = t.running ? "Pause" : "Start";
    ui.trafficButton.className = t.running ? "" : "primary";
    if (document.activeElement !== ui.trafficRate) ui.trafficRate.value = String(t.rate);
    ui.trafficTarget.textContent = `DogStatsD → udp://${t.target}`;
    const activity = t.running ? `Sending ${rate(t.rate)} datagrams/s` : "Paused";
    const lines = [el("div", { text: `${activity} · ${count(t.datagrams_sent)} sent` })];
    if (t.send_errors) {
      lines.push(el("div", { class: "errors", text: `${count(t.send_errors)} send errors: ${t.last_error || ""}` }));
    }
    ui.trafficStats.replaceChildren(...lines);
    renderCanary(s.canary);
  }

  // The canary check compares the canaries sent in a recent range of seconds with those the primary endpoint
  // received for the same seconds, so it follows the present instead of adding up everything since the demo started.
  function renderCanary(c) {
    const range = `${clock(c.from_s * 1000)}–${clock(c.to_s * 1000)}`;
    const parts = [];
    if (c.state === "waiting") {
      parts.push(el("span", { class: "muted", text: `Canary check waiting: ${c.reason || ""}` }));
    } else if (c.state === "idle") {
      parts.push(el("span", { class: "muted", text: `Canary: nothing sent ${range}.` }));
    } else {
      parts.push(`Canary ${range}: sent `, el("strong", { text: count(c.sent) }), " · primary received ",
        el("strong", { text: count(c.received) }), " ");
      const difference = c.received - c.sent;
      if (c.state === "balanced") parts.push(el("span", { class: "ok", text: "✓ none lost" }));
      else if (c.state === "missing") parts.push(el("span", { class: "err", text: `${count(-difference)} missing` }));
      else parts.push(el("span", { class: "warn", text: `${count(difference)} more than sent` }));
    }
    if (c.elsewhere.length) {
      parts.push(` · also at ${c.elsewhere.map((e) => `${e.intake} ${count(e.received)}`).join(", ")}`);
    }
    ui.canary.replaceChildren(...parts);
    ui.canary.title = `Every datagram carries one demo.canary. Compares the canaries sent in ${range} with those the `
      + `primary endpoint received for those seconds, allowing ${count(c.allowance)} for canaries counted in a `
      + "neighboring second.";
  }

  function seriesLabel(entry, name) {
    return entry.labels[name] || "";
  }

  function renderNodes(s) {
    const t = s.telemetry;
    const stale = t.state !== "ok";
    const shown = (value) => (stale ? "—" : rate(value));
    const plan = t.plan;
    const planParts = [];
    const gauge = (name) => plan.find((e) => e.name === name);
    const gauges = [
      ["plan_active_generation", "generation"],
      ["plan_active_revision", "revision"],
      ["plan_live_generations", "live generations"],
    ];
    for (const [name, label] of gauges) {
      const entry = gauge(name);
      if (entry) planParts.push([label, count(entry.value)]);
    }
    const installs = plan.filter((e) => e.name === "plan_installs_total")
      .map((e) => `${seriesLabel(e, "outcome") || "?"} ${count(e.value)}`);
    if (installs.length) planParts.push(["installs", installs.join(", ")]);
    const losses = plan.filter((e) => e.name === "plan_loss_total" && e.rate > 0)
      .map((e) => `${seriesLabel(e, "class")} ${rate(e.rate)}/s at ${seriesLabel(e, "component_id")}`);
    if (losses.length && !stale) planParts.push(["loss", losses.join(", ")]);
    ui.planStats.replaceChildren(...planParts.map(([k, v]) => el("span", {}, `${k} `, el("strong", { text: v }))));

    nodesTable.classList.toggle("stale", stale);
    ui.nodes.replaceChildren();
    let hint = "";
    if (t.state === "waiting") hint = "waiting for the first scrape of ADP's /metrics";
    else if (stale) hint = `ADP's /metrics is unreachable (${t.error || "no answer"}): last known values, greyed`;
    if (t.nodes.length) {
      ui.nodesTitle.textContent = "Per-node rates";
      ui.nodesHint.textContent = hint || "plan_node_events_total, events per second, averaged over 5 s";
      const headings = ["Node", "Port", "Events/s", "Last 30 s"].map((h) => el("th", { text: h }));
      ui.nodesHead.replaceChildren(el("tr", {}, ...headings));
      for (const e of t.nodes) {
        ui.nodes.append(el("tr", {},
          el("td", { class: "id", text: seriesLabel(e, "component_id") }),
          el("td", { class: "mono", text: seriesLabel(e, "port") }),
          el("td", { class: "num", text: shown(e.rate) }),
          el("td", {}, sparkline(smooth(e.history)))));
      }
      return;
    }
    // Before the plan-driven topology, per-component native telemetry is what there is.
    ui.nodesTitle.textContent = "Per-component rates";
    const byComponent = new Map();
    for (const e of t.components) {
      const id = seriesLabel(e, "component_id");
      if (!id) continue;
      const row = byComponent.get(id) || { received: 0, sent: 0, dropped: 0, errors: 0, history: null };
      if (e.name === "component_events_received_total") {
        row.received += e.rate;
        row.history = e.history;
      } else if (e.name === "component_events_sent_total") {
        row.sent += e.rate;
        if (!row.history) row.history = e.history;
      } else if (e.name === "component_events_dropped_total") {
        row.dropped += e.rate;
      } else if (e.name === "component_errors_total") {
        row.errors += e.rate;
      }
      byComponent.set(id, row);
    }
    const delivered = el("th", { text: "Delivered/s" });
    delivered.title = "Events sent to downstream components, counted once per consumer, as ADP's telemetry counts them";
    const headings = [el("th", { text: "Component" }), el("th", { text: "In/s" }), delivered,
      el("th", { text: "Dropped/s" }), el("th", { text: "Last 30 s" })];
    ui.nodesHead.replaceChildren(el("tr", {}, ...headings));
    // Components with no traffic in the whole history (events, service checks, ...) are left out.
    const active = (row) => row.received || row.sent || row.dropped || (row.history || []).some((v) => v > 0);
    const all = [...byComponent.keys()].sort();
    const ids = all.filter((id) => active(byComponent.get(id)));
    const hidden = all.length > ids.length ? ` · ${all.length - ids.length} idle components hidden` : "";
    ui.nodesHint.textContent = (hint || "native component telemetry, averaged over 5 s; per-node plan telemetry "
      + "appears with the plan-driven topology") + hidden;
    if (!ids.length) {
      const text = stale ? "No component telemetry: ADP is not answering." : "No component telemetry yet.";
      ui.nodes.append(el("tr", {}, el("td", { colspan: "5", class: "detail", text })));
    }
    for (const id of ids) {
      const row = byComponent.get(id);
      ui.nodes.append(el("tr", {},
        el("td", { class: "id", text: id }),
        el("td", { class: "num", text: shown(row.received) }),
        el("td", { class: "num", text: shown(row.sent) }),
        el("td", { class: "num", text: row.dropped ? shown(row.dropped) : "" }),
        el("td", {}, row.history ? sparkline(smooth(row.history)) : "")));
    }
  }

  function renderFoot(s) {
    const parts = [
      ["run directory", s.run_dir],
      ["ADP log", s.adp.log_path],
      ["ADP console", s.adp.console_log_path],
    ];
    if (s.adp.phase === "external") parts.push(["start ADP with", s.adp.command]);
    ui.foot.replaceChildren(...parts.flatMap(([k, v], i) => [i ? " · " : "", `${k} `, el("code", { text: v })]));
  }

  function render(s) {
    renderBadges(s);
    renderBanner(s);
    renderBindings(s);
    renderIntakes(s);
    renderOutcome(s);
    renderAcks(s);
    renderTraffic(s);
    renderNodes(s);
    renderFoot(s);
    renderPreview();
    renderPublish();
    followExport(s);
  }

  let polling = false;
  async function poll() {
    if (polling) return;
    polling = true;
    try {
      const { ok, data } = await api("GET", "/api/state");
      if (ok && data) {
        state = data;
        firstSeenMs = Math.min(firstSeenMs, data.now_ms);
        render(data);
      }
    } catch (_) {
      ui.banner.replaceChildren(el("strong", { text: "The demo server is not answering." }),
        " Is programmable-metrics-demo still running?");
      ui.banner.className = "banner err";
      ui.banner.hidden = false;
    } finally {
      polling = false;
    }
  }

  // ---- Start ----------------------------------------------------------------------------------------------------

  renderEditor();
  loadDefault(true);
  poll();
  setInterval(poll, POLL_MS);
})();
