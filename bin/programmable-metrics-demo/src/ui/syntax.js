// Syntax highlighting of the pipeline syntax, shared by the editor and the language reference.
//
// `PipelineSyntax.highlight(text)` returns the text as HTML, each token in a span whose class names its kind:
// `tk-comment`, `tk-string`, `tk-number` (numbers and durations), `tk-kw` (the keywords: `policy`, `let`, `where`,
// `fork`, `emit`, `output` after `emit`, `drop`, `exit`, and the header's `base`, `outputs`, `blobs`, `ir_version`),
// `tk-pipe` (`|`), `tk-logic` (`and`, `or`, `not`, `in`, `starts_with`, `is`, and `set` and `unset` after `is`),
// `tk-op` (`==`, `!=`), `tk-const` (`true`, `false`, `null`), `tk-fn` (an operator or function: the name of a stage,
// or a name followed by `(`), `tk-param` (a parameter or record member's name, before its `:`), `tk-config` (a
// binding, `config.<name>`), and `tk-as` (an `as <id>` clause, which only keeps a node's ID). Keywords are reserved
// only where the grammar expects them, so a field such as `tags.drop` or a parameter `output:` is not one. The text
// itself is unchanged: highlighting only wraps it.
"use strict";

window.PipelineSyntax = (() => {
  const KEYWORDS = new Set([
    "policy", "let", "where", "fork", "emit", "drop", "exit", "base", "outputs", "blobs", "ir_version",
  ]);
  const LOGIC = new Set(["and", "or", "not", "in", "starts_with", "is"]);
  const CONSTANTS = new Set(["true", "false", "null"]);
  const IDENT = /[A-Za-z_][A-Za-z0-9_]*/y;
  const NUMBER = /-?\d+(?:\.\d+)?(?:ns|us|ms|s|m|h)?(?![A-Za-z0-9_])/y;
  const STRING = /"(?:[^"\\\n]|\\.)*"?/y;
  const COMMENT = /#[^\n]*/y;
  const AS = /as(\s+)([A-Za-z_][A-Za-z0-9_]*)/y;
  const BINDING = /config(?:\.[A-Za-z0-9_]+)+/y;

  function esc(text) {
    return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  }

  function match(re, text, at) {
    re.lastIndex = at;
    const found = re.exec(text);
    return found ? found[0] : null;
  }

  // Returns the next character at or after `at` that is not a space or a tab.
  function nextChar(text, at) {
    while (at < text.length && (text[at] === " " || text[at] === "\t")) at++;
    return text[at] || "";
  }

  function highlight(text) {
    const out = [];
    const span = (kind, token) => out.push(`<span class="${kind}">${esc(token)}</span>`);
    let at = 0;
    // The last token that was no whitespace or comment, which decides what a name is.
    let previous = "";
    while (at < text.length) {
      const c = text[at];
      let token;
      if (c === "#") {
        token = match(COMMENT, text, at);
        span("tk-comment", token);
        at += token.length;
        continue;
      }
      if (c === " " || c === "\t" || c === "\n" || c === "\r") {
        out.push(c);
        at += 1;
        continue;
      }
      if (c === "\"") {
        token = match(STRING, text, at);
        span("tk-string", token);
      } else if (c === "|") {
        token = "|";
        span("tk-pipe", token);
      } else if ((c === "=" || c === "!") && text[at + 1] === "=") {
        token = text.slice(at, at + 2);
        span("tk-op", token);
      } else if (text[at - 1] !== "." && (/[0-9]/.test(c) || (c === "-" && /[0-9]/.test(text[at + 1] || "")))) {
        token = match(NUMBER, text, at) || c;
        span("tk-number", token);
      } else if (/[A-Za-z_]/.test(c)) {
        const field = text[at - 1] === ".";
        const binding = !field && match(BINDING, text, at);
        // `as` then whitespace then a name: the clause, whatever its spacing.
        const clause = !field && match(AS, text, at);
        token = match(IDENT, text, at);
        const after = nextChar(text, at + token.length);
        if (binding) {
          token = binding;
          span("tk-config", token);
        } else if (clause && token === "as" && after !== ":") {
          token = clause;
          span("tk-as", token);
          previous = "as";
          at += token.length;
          continue;
        } else if (field) {
          out.push(esc(token));
        } else if (after === ":" && !text.startsWith("::", at + token.length)) {
          span("tk-param", token);
        } else if (token === "output" && previous === "emit") {
          span("tk-kw", token);
        } else if ((token === "set" || token === "unset") && previous === "is") {
          span("tk-logic", token);
        } else if (KEYWORDS.has(token)) {
          span("tk-kw", token);
        } else if (LOGIC.has(token)) {
          span("tk-logic", token);
        } else if (CONSTANTS.has(token)) {
          span("tk-const", token);
        } else if (after === "(" || previous === "|") {
          span("tk-fn", token);
        } else {
          out.push(esc(token));
        }
      } else {
        token = c;
        out.push(esc(c));
      }
      previous = token;
      at += token.length;
    }
    return out.join("");
  }

  return { highlight };
})();
