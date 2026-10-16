// The language reference: renders what the demo server builds from the operator registry, the validator, and ADP's
// export (see `reference.rs`). The grammar's prose is in the page; everything that can be generated is generated, so
// the page shows what this ADP accepts.
"use strict";

const $ = (id) => document.getElementById(id);

function esc(text) {
  return String(text)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

// Text from the server marks code with backticks, as Rust documentation does.
const prose = (text) => esc(text).replace(/`([^`]+)`/g, "<code>$1</code>");
const code = (text) => `<code>${esc(text)}</code>`;
const pre = (text) => `<pre>${esc(text)}</pre>`;
const tag = (text, kind = "") => `<span class="tag ${kind}">${esc(text)}</span>`;

function renderHost(host, missing) {
  if (!host) {
    return `<p class="host-note">${esc(missing || "ADP's export is not at hand.")} The inputs, recipients, and
      bindings below appear once it is; the rest of this page does not depend on it.</p>`;
  }
  const inputs = host.inputs.length
    ? host.inputs.map((name) => code(`input.${name}`)).join(", ")
    : '<span class="muted">none</span>';
  const recipients = host.recipients.length
    ? `<table><thead><tr><th>Name in <code>emit to [...]</code></th><th>Endpoint</th><th></th></tr></thead><tbody>
        ${host.recipients
          .map((r) => `<tr><td>${code(r.name)}</td><td>${code(r.endpoint)}</td>
            <td>${r.label ? esc(r.label) : ""}</td></tr>`)
          .join("")}
      </tbody></table>`
    : '<p class="muted">The directory is empty: name outputs with <code>emit &lt;output&gt;</code>.</p>';
  const bindable = host.bindings.filter((b) => b.bindable);
  const policy = host.bindings.filter((b) => !b.bindable);
  const bindingRows = (rows) => rows
    .map((b) => `<tr><td>${code(`config("${b.name}")`)}</td><td>${tag(b.class)}${b.live ? " " + tag("live", "ok") : ""}</td>
      <td>${code(b.type)}</td><td>${b.value === undefined ? "" : code(JSON.stringify(b.value))}</td></tr>`)
    .join("");
  const head = "<thead><tr><th>Binding</th><th>Kind</th><th>Type</th><th>Now</th></tr></thead>";
  return `
    <p>The program replaces the scope ${code(host.scope)}.
      ${host.single_aggregation ? "It may hold at most one <code>aggregate</code>." : ""}</p>
    <h3>Inputs</h3>
    <p>${inputs}. An input with no edge receives nothing.</p>
    <h3>Recipients</h3>
    <p>The names <code>emit to [...]</code> takes. Adding a destination in the demo adds one here, by its URL.</p>
    ${recipients}
    <h3>Bindings a program may read</h3>
    <p>Facts about the host and signals that change as ADP runs. A <span class="tag ok">live</span> one changes
      what the program does without a new program.</p>
    ${bindable.length ? `<table>${head}<tbody>${bindingRows(bindable)}</tbody></table>`
      : '<p class="muted">None.</p>'}
    ${policy.length && host.detached ? `
      <details><summary>Policy settings (${policy.length}): written as literals, not bound</summary>
        <p>These native settings feed only the default program. A published program owns its policy, so it writes
          the value it wants instead of <code>config("...")</code>; compiling refuses a program that binds one.</p>
        <table>${head}<tbody>${bindingRows(policy)}</tbody></table>
      </details>` : ""}`;
}

function renderOperator(op) {
  const params = op.params.length
    ? `<table><thead><tr><th>Parameter</th><th>Type</th></tr></thead><tbody>
        ${op.params
          .map((p) => `<tr><td>${code(p.name + (p.required ? "" : "?"))}${p.bindable ? " " + tag("config") : ""}</td>
            <td>${code(p.type)}</td></tr>`)
          .join("")}
      </tbody></table>`
    : '<p class="muted">No parameters.</p>';
  const ports = op.inputs.length || op.outputs.length
    ? `<p>Ports in: ${op.inputs.map(code).join(", ") || "none"}; out: ${op.outputs.map(code).join(", ") || "none"}.
        ${op.ports_note ? prose(op.ports_note) : ""}</p>`
    : op.ports_note ? `<p>${prose(op.ports_note)}</p>` : "";
  return `<div class="op" id="op-${esc(op.name)}">
    <div class="op-head">${code(op.name)}${op.authorable ? "" : " " + tag("native configuration only", "warn")}</div>
    <p>${prose(op.summary)}</p>
    ${params}
    ${ports}
    ${op.example ? pre(op.example) : ""}
  </div>`;
}

function render(reference) {
  $("example-program").textContent = reference.example_program;
  $("host-body").innerHTML = renderHost(reference.host, reference.host_missing);
  $("predicates-body").innerHTML = reference.predicates
    .map((p) => `<tr><td>${prose(p.tests)}</td><td>${code(p.example)}</td></tr>`)
    .join("");
  const authorable = reference.operators.filter((op) => op.authorable);
  const host = reference.operators.filter((op) => !op.authorable);
  $("operators-body").innerHTML = authorable.map(renderOperator).join("")
    + (host.length ? `<h3>Placed by native configuration only</h3>
        <p>A program cannot add, change, or remove these: ADP builds them at boot.</p>
        ${host.map(renderOperator).join("")}` : "");
  $("recipes-body").innerHTML = reference.recipes
    .map((r) => `<h3>${esc(r.title)}</h3><p>${prose(r.how)}</p>${pre(r.source)}`)
    .join("");
  $("rules-body").innerHTML = reference.rules
    .map((r) => `<tr><td>${code(r.name)}</td><td>${tag(r.severity, r.severity === "error" ? "err" : "warn")}</td>
      <td>${prose(r.means)}</td></tr>`)
    .join("");
  $("limits-body").innerHTML = reference.limits
    .map((l) => `<tr><td>${code(l.name)}</td><td>${esc(l.value)}</td><td>${prose(l.description)}</td></tr>`)
    .join("");
  if (location.hash) {
    document.getElementById(location.hash.slice(1))?.scrollIntoView();
  }
}

fetch("api/reference")
  .then((response) => {
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return response.json();
  })
  .then(render)
  .catch((error) => {
    $("host-body").innerHTML = `<p class="host-note">The reference could not be loaded: ${esc(error.message)}</p>`;
  });
