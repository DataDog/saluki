// The language reference: renders what the demo server builds from the operator registry, the validator, the parser,
// and ADP's export (see `reference.rs`). The model's and the grammar's prose is here and in the page; everything that
// can be generated is generated, so the page shows what this ADP accepts.
"use strict";

const $ = (id) => document.getElementById(id);

function esc(text) {
  return String(text)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// Program text, highlighted as the editor highlights it.
const highlighted = (text) => window.PipelineSyntax.highlight(String(text));
// Text from the server marks code with backticks, as Rust documentation does.
const prose = (text) => esc(text).replace(/`([^`]+)`/g, "<code>$1</code>");
const code = (text) => `<code>${esc(text)}</code>`;
const program = (text) => `<code>${highlighted(text)}</code>`;
const pre = (text) => `<pre><code>${highlighted(text)}</code></pre>`;
const tag = (text, kind = "") => `<span class="tag ${kind}">${esc(text)}</span>`;

// The ideas of the language, each with a line of it.
const CONCEPTS = [
  ["Streams", "dogstatsd", "Each source's metrics enter as an input of the scope: a stream a pipeline starts from."],
  ["Stages", "dogstatsd | namespace_prefix(\"demo\", exclusions: [])",
    "`|` feeds a stream to an operator. Each stage is a node of the plan, and its output a new stream."],
  ["Sinks", "... | emit primary\n... | emit output dsd_client_telemetry\n... | drop",
    "End a pipeline: deliver to recipients of the directory or to an output, or discard."],
  ["let", "let dsd = dogstatsd\n  | aggregate(window: 10s, flush_interval: 10s)",
    "Names a stream, for later statements to use. A `let` never ends in a sink; a statement without one always does."],
  ["Naming a stream", "let metrics = dsd\nmetrics | emit primary\nmetrics | emit mrf",
    "`let x = y`, with no stages, gives the stream `y` another name and adds no node. Like any stream, `x` used " +
    "more than once is copied to each use."],
  ["Ports", "let by_time = dogstatsd | timestamp_split(10s)\nby_time.passthrough | drop",
    "A node with several output ports is read one port at a time, as `<name>.<port>`, and every port is used."],
  ["Reuse", "metrics | emit primary\nmetrics | where tags.team == \"b\" | emit mrf",
    "A stream used more than once is copied to each use. This is how a program fans out: each use is a " +
    "statement of its own, a branch that gets every metric."],
  ["Tuples", "(dsd, otlp) | host_tags([\"env:demo\"], deadline: 30s)",
    "Merge streams: the first into the merge's port 0, the next into port 1, and so on."],
];

const GRAMMAR = `program    = "policy" scope "{" header* statement* "}"
header     = "ir_version" int | "base" hash [ "outputs" "{" (output "=" hash ","?)* "}" ] | "blobs" "{" ... "}"
statement  = "let" ident "=" pipeline                   # a named stream
           | pipeline                                   # must end in a sink
pipeline   = head ("|" stage)* ["|" sink]
head       = ref | tuple
ref        = ident ["." port]                           # a let, or a scope input; ".port" picks an output port
tuple      = "(" ref ("," ref)* ","? ")" ["as" ident]   # a merge, its input ports in order
stage      = operator ["(" args ")"] ["as" ident]       # any registered operator
           | "where" predicate ["as" ident]             # shorthand
sink       = "emit" recipients ["as" ident]             # emit primary, emit [primary, "https://b.example.com"]
           | "emit" "output" output ["as" ident]
           | "drop" ["as" ident]
           | "exit" exit-name ["as" ident]              # leaves a scope that ends early
args       = [positional ","] (param ":" value ","?)*`;

function renderConcepts() {
  $("concepts").innerHTML = CONCEPTS
    .map(([name, sample, means]) => `<div class="concept"><strong>${esc(name)}</strong>
      <code class="sample">${highlighted(sample)}</code><p>${prose(means)}</p></div>`)
    .join("");
  $("grammar-text").textContent = GRAMMAR;
}

function renderHost(host, missing) {
  if (!host) {
    return `<p class="host-note">${esc(missing || "ADP's export is not at hand.")} The inputs, recipients, and
      bindings below appear once it is; the rest of this page does not depend on it.</p>`;
  }
  const inputs = host.inputs.length
    ? host.inputs.map(code).join(", ")
    : '<span class="muted">none</span>';
  const recipients = host.recipients.length
    ? `<table><thead><tr><th>Name in <code>emit</code></th><th>Endpoint</th><th></th></tr></thead><tbody>
        ${host.recipients
          .map((r) => `<tr><td>${program(r.name)}</td><td>${code(r.endpoint)}</td>
            <td>${r.label ? esc(r.label) : ""}</td></tr>`)
          .join("")}
      </tbody></table>`
    : '<p class="muted">The directory is empty: name outputs with <code>emit output &lt;output&gt;</code>.</p>';
  const bindingRows = (rows) => rows
    .map((b) => `<tr><td>${program(`config.${b.name}`)}</td>
      <td>${tag(b.class)}${b.live ? " " + tag("live", "ok") : ""}</td>
      <td>${code(b.type)}</td><td>${b.value === undefined ? "" : code(JSON.stringify(b.value))}</td></tr>`)
    .join("");
  const head = "<thead><tr><th>Binding</th><th>Kind</th><th>Type</th><th>Now</th></tr></thead>";
  return `
    <p>The program replaces the scope ${code(host.scope)}.
      ${host.single_aggregation ? "It may hold at most one <code>aggregate</code>." : ""}</p>
    <h3>Inputs</h3>
    <p>${inputs}. Each is a stream a pipeline may start from, as <code>dogstatsd | ...</code>; an input no pipeline
      reads receives nothing.</p>
    <h3>Recipients</h3>
    <p>The names <code>emit</code> takes: one bare, as <code>emit primary</code>, or several in brackets, as
      <code>emit [primary, "http://..."]</code>. Adding a destination in the demo adds one here, by its URL.</p>
    ${recipients}
    <h3>Bindings a program may read</h3>
    <p>Facts about the host and signals that change as ADP runs. A <span class="tag ok">live</span> one changes
      what the program does without a new program.</p>
    ${host.bindings.length ? `<table>${head}<tbody>${bindingRows(host.bindings)}</tbody></table>`
      : '<p class="muted">None.</p>'}
    <p>Policy settings, such as the filterlist, are never bound: a program writes the values it wants, as the
      default program writes native configuration's.</p>`;
}

function renderSugar(sugar) {
  $("sugar-body").innerHTML = sugar
    .map((s) => `<tr><td><div class="form">${prose(s.form)}</div><p class="means">${prose(s.means)}</p></td>
      <td>${pre(s.sugar)}</td><td>${pre(s.core)}</td></tr>`)
    .join("");
}

function renderParam(p) {
  const flags = [
    p.positional ? tag("positional", "plain") : "",
    p.required ? "" : tag("optional", "plain"),
    p.bindable ? tag("config") : "",
  ].filter(Boolean).join(" ");
  return `<tr><td>${code(p.name)}${flags ? `<span class="flags">${flags}</span>` : ""}</td>
    <td>${code(p.type)}</td><td>${p.default === undefined ? "" : program(p.default)}</td></tr>`;
}

function renderOperator(op) {
  const params = op.params.length
    ? `<table><thead><tr><th>Parameter</th><th>Type</th><th>Default</th></tr></thead><tbody>
        ${op.params.map(renderParam).join("")}
      </tbody></table>`
    : '<p class="muted">No parameters: written without parentheses.</p>';
  const ports = op.inputs.length || op.outputs.length
    ? `<p class="ports">Ports in: ${op.inputs.map(code).join(", ") || "none"}; out:
        ${op.outputs.map(code).join(", ") || "none"}.</p>`
    : "";
  const note = op.ports_note ? `<p>${prose(op.ports_note)}</p>` : "";
  return `<div class="op" id="op-${esc(op.name)}">
    <div class="op-head">${code(op.name)}${op.authorable ? "" : " " + tag("native configuration only", "warn")}</div>
    <p>${prose(op.summary)}</p>
    <div class="op-body">
      <div>${params}</div>
      <div>${op.example ? pre(op.example) : ""}${ports}${note}</div>
    </div>
  </div>`;
}

function renderExampleIds(nodes) {
  if (!nodes.length) {
    $("example-ids").innerHTML = `<p class="muted">The node IDs of the <a href="#example">whole program</a> above
      are listed here once the demo's parser reads it.</p>`;
    return;
  }
  $("example-ids").innerHTML = `<h3>In the whole program above</h3>
    <p>The nodes the <a href="#example">example</a> describes, with the IDs the syntax gives them:</p>
    <ul class="example-nodes">${nodes
      .map((n) => `<li>${esc(n.id)} <span class="op-name">${esc(n.op)}</span></li>`)
      .join("")}</ul>`;
}

function renderRecipe(r) {
  return `<div class="recipe"><h3>${esc(r.title)}</h3><p>${prose(r.how)}</p>
    <div class="insert" aria-label="The line, in its place">
      <div class="ctx">    ...</div>
      <div class="ctx">${highlighted(r.above)}</div>
      <div class="add">${highlighted(r.line)}</div>
      <div class="ctx">${highlighted(r.below)}</div>
      <div class="ctx">    ...</div>
    </div></div>`;
}

function render(reference) {
  $("example-program").innerHTML = `<code>${highlighted(reference.example_program)}</code>`;
  $("host-body").innerHTML = renderHost(reference.host, reference.host_missing);
  renderSugar(reference.sugar);
  $("predicates-body").innerHTML = reference.predicates
    .map((p) => `<tr><td>${prose(p.tests)}</td><td>${program(p.example)}</td></tr>`)
    .join("");
  const authorable = reference.operators.filter((op) => op.authorable);
  const host = reference.operators.filter((op) => !op.authorable);
  $("op-index").innerHTML = reference.operators
    .map((op) => `<a href="#op-${esc(op.name)}" class="${op.authorable ? "" : "host"}">${esc(op.name)}</a>`)
    .join("");
  $("operators-body").innerHTML = authorable.map(renderOperator).join("")
    + (host.length ? `<h3>Placed by native configuration only</h3>
        <p>A program cannot add, change, or remove these: ADP builds them at boot.</p>
        ${host.map(renderOperator).join("")}` : "");
  renderExampleIds(reference.example_nodes || []);
  $("recipes-body").innerHTML = reference.recipes.map(renderRecipe).join("");
  $("rules-body").innerHTML = reference.rules
    .map((r) => `<tr class="${r.met_by_syntax ? "rule-met" : ""}"><td>${code(r.name)}</td>
      <td>${tag(r.severity, r.severity === "error" ? "err" : "warn")}
        ${r.met_by_syntax ? tag("met by the syntax", "ok") : ""}</td>
      <td>${prose(r.means)}</td></tr>`)
    .join("");
  $("limits-body").innerHTML = reference.limits
    .map((l) => `<tr><td>${code(l.name)}</td><td>${esc(l.value)}</td><td>${prose(l.description)}</td></tr>`)
    .join("");
  if (location.hash) {
    document.getElementById(location.hash.slice(1))?.scrollIntoView();
  }
}

renderConcepts();
fetch("api/reference")
  .then((response) => {
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
  })
  .then(render)
  .catch((error) => {
    $("host-body").innerHTML = `<p class="host-note">The reference could not be loaded: ${esc(error.message)}</p>`;
  });
