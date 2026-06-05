//! Generates the ADP configuration documentation Markdown from a [`SchemaOverlay`].
//!
//! [`generate`] renders a TinyTemplate source with Markdown tables derived from each overlay
//! section and writes the result to `$OUT_DIR/docs/dogstatsd.md`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use datadog_agent_config_overlay_model::{Investigate, SchemaOverlay, SupportLevel, Supported, Unsupported};
use tinytemplate::TinyTemplate;

const ISSUE_BASE_URL: &str = "https://github.com/DataDog/saluki/issues/";

// ── Table rendering ─────────────────────────────────────────────────────────

struct TwoColRow {
    key: String,
    description: String,
}

struct ThreeColRow {
    key: String,
    description: String,
    extra: String,
}

fn render_two_col_table(headers: [&str; 2], rows: &[TwoColRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let w0 = rows.iter().map(|r| r.key.len()).max().unwrap().max(headers[0].len());
    let w1 = rows
        .iter()
        .map(|r| r.description.len())
        .max()
        .unwrap()
        .max(headers[1].len());
    let mut out = String::new();
    writeln!(out, "| {:<w0$} | {:<w1$} |", headers[0], headers[1]).unwrap();
    writeln!(out, "| {:-<w0$} | {:-<w1$} |", "", "").unwrap();
    for row in rows {
        writeln!(out, "| {:<w0$} | {:<w1$} |", row.key, row.description).unwrap();
    }
    out
}

fn render_three_col_table(headers: [&str; 3], rows: &[ThreeColRow]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let w0 = rows.iter().map(|r| r.key.len()).max().unwrap().max(headers[0].len());
    let w1 = rows
        .iter()
        .map(|r| r.description.len())
        .max()
        .unwrap()
        .max(headers[1].len());
    let w2 = rows.iter().map(|r| r.extra.len()).max().unwrap().max(headers[2].len());
    let mut out = String::new();
    writeln!(
        out,
        "| {:<w0$} | {:<w1$} | {:<w2$} |",
        headers[0], headers[1], headers[2]
    )
    .unwrap();
    writeln!(out, "| {:-<w0$} | {:-<w1$} | {:-<w2$} |", "", "", "").unwrap();
    for row in rows {
        writeln!(
            out,
            "| {:<w0$} | {:<w1$} | {:<w2$} |",
            row.key, row.description, row.extra
        )
        .unwrap();
    }
    out
}

// ── Issue reference handling ────────────────────────────────────────────────

fn parse_issue_number(issue: &str) -> Option<u64> {
    issue.strip_prefix('#').and_then(|s| s.parse().ok())
}

fn collect_issue(issue: &Option<String>, issues: &mut BTreeMap<u64, String>) -> String {
    match issue {
        Some(i) => {
            if let Some(n) = parse_issue_number(i) {
                issues.entry(n).or_insert_with(|| i.clone());
            }
            format!("[{}]", i)
        }
        None => String::new(),
    }
}

// ── Documentation block rendering ───────────────────────────────────────────

fn render_docs_block(entries: &[(&str, &str)]) -> String {
    let mut out = String::new();
    for (key, doc) in entries {
        let trimmed = doc.trim();
        if !trimmed.starts_with('#') {
            writeln!(out, "### `{}`", key).unwrap();
            writeln!(out).unwrap();
        }
        writeln!(out, "{}", trimmed).unwrap();
        writeln!(out).unwrap();
    }
    out
}

// ── Public entry point ──────────────────────────────────────────────────────

pub fn generate(overlay: &SchemaOverlay, template_path: &Path, out_dir: &Path) {
    let template_src = std::fs::read_to_string(template_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {}", template_path.display(), e));

    let mut issues: BTreeMap<u64, String> = BTreeMap::new();

    // ── Being Worked On (unsupported, planned: true) ────────────────────
    let working_on: Vec<(&str, &Unsupported)> = overlay
        .unsupported
        .iter()
        .filter(|(_, u)| u.planned)
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    let working_on_rows: Vec<ThreeColRow> = working_on
        .iter()
        .map(|(key, u)| ThreeColRow {
            key: format!("`{}`", key),
            description: u.description.clone(),
            extra: collect_issue(&u.issue, &mut issues),
        })
        .collect();
    let working_on_table = render_three_col_table(["Config Key", "Description", "Issue"], &working_on_rows);

    // ── Not Planned (unsupported, planned: false) ───────────────────────
    let not_planned: Vec<(&str, &Unsupported)> = overlay
        .unsupported
        .iter()
        .filter(|(_, u)| !u.planned)
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    let not_planned_rows: Vec<ThreeColRow> = not_planned
        .iter()
        .map(|(key, u)| {
            if let Some(i) = &u.issue {
                if let Some(n) = parse_issue_number(i) {
                    issues.entry(n).or_insert_with(|| i.clone());
                }
            }
            let reason = match u.documentation.as_deref() {
                Some(d) if d.contains('\n') => "See below".to_string(),
                Some(d) => d.to_string(),
                None => String::new(),
            };
            ThreeColRow {
                key: format!("`{}`", key),
                description: u.description.clone(),
                extra: reason,
            }
        })
        .collect();
    let not_planned_table = render_three_col_table(["Config Key", "Description", "Reason"], &not_planned_rows);

    let not_planned_doc_entries: Vec<(&str, &str)> = not_planned
        .iter()
        .filter_map(|(key, u)| {
            u.documentation
                .as_deref()
                .filter(|d| d.contains('\n'))
                .map(|d| (*key, d))
        })
        .collect();
    let not_planned_docs = render_docs_block(&not_planned_doc_entries);

    // ── Behavioral Differences (supported, partial) ─────────────────────
    let behavioral: Vec<(&str, &Supported)> = overlay
        .supported
        .iter()
        .filter(|(_, s)| s.support_level == SupportLevel::Partial)
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    let behavioral_rows: Vec<TwoColRow> = behavioral
        .iter()
        .map(|(key, s)| {
            if let Some(i) = &s.issue {
                if let Some(n) = parse_issue_number(i) {
                    issues.entry(n).or_insert_with(|| i.clone());
                }
            }
            TwoColRow {
                key: format!("`{}`", key),
                description: s.description.clone(),
            }
        })
        .collect();
    let behavioral_table = render_two_col_table(["Config Key", "Description"], &behavioral_rows);

    let behavioral_doc_entries: Vec<(&str, &str)> = behavioral
        .iter()
        .filter_map(|(key, s)| s.documentation.as_deref().map(|d| (*key, d)))
        .collect();
    let behavioral_docs = render_docs_block(&behavioral_doc_entries);

    // ── Compatibility Unknown (investigate) ─────────────────────────────
    let inv_entries: Vec<(&str, &Investigate)> = overlay.investigate.iter().map(|(k, v)| (k.as_str(), v)).collect();

    let inv_rows: Vec<ThreeColRow> = inv_entries
        .iter()
        .map(|(key, inv)| ThreeColRow {
            key: format!("`{}`", key),
            description: inv.description.clone(),
            extra: collect_issue(&inv.issue, &mut issues),
        })
        .collect();
    let investigate_table = render_three_col_table(["Config Key", "Description", "Issue"], &inv_rows);

    // ── ADP-Only Settings (saluki_keys) ──────────────────────────────────
    let mut adp_rows: Vec<ThreeColRow> = datadog_agent_config_overlay_model::saluki_keys::SALUKI_KEYS
        .iter()
        .map(|sk| ThreeColRow {
            key: format!("`{}`", sk.yaml_path),
            description: sk.description.to_string(),
            extra: if sk.default.is_empty() {
                String::new()
            } else {
                sk.default.to_string()
            },
        })
        .collect();
    adp_rows.sort_by(|a, b| a.key.cmp(&b.key));
    let adp_only_table = render_three_col_table(["Config Key", "Description", "Default"], &adp_rows);

    let adp_doc_entries: Vec<(&str, &str)> = datadog_agent_config_overlay_model::saluki_keys::SALUKI_KEYS
        .iter()
        .filter_map(|sk| sk.documentation.map(|d| (sk.yaml_path, d)))
        .collect();
    let adp_only_docs = render_docs_block(&adp_doc_entries);

    // ── Transparent Settings (supported, full) ──────────────────────────
    let transparent: Vec<(&str, &Supported)> = overlay
        .supported
        .iter()
        .filter(|(_, s)| s.support_level == SupportLevel::Full)
        .map(|(k, v)| (k.as_str(), v))
        .collect();

    let transparent_rows: Vec<TwoColRow> = transparent
        .iter()
        .map(|(key, s)| {
            if let Some(i) = &s.issue {
                if let Some(n) = parse_issue_number(i) {
                    issues.entry(n).or_insert_with(|| i.clone());
                }
            }
            TwoColRow {
                key: format!("`{}`", key),
                description: s.description.clone(),
            }
        })
        .collect();
    let transparent_table = render_two_col_table(["Config Key", "Description"], &transparent_rows);

    // ── Issue references ────────────────────────────────────────────────
    let mut issue_refs = String::new();
    for (n, raw) in &issues {
        writeln!(issue_refs, "[{}]: {}{}", raw, ISSUE_BASE_URL, n).unwrap();
    }

    // ── Render template ─────────────────────────────────────────────────
    let mut ctx: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    ctx.insert("working_on_table".to_string(), working_on_table);
    ctx.insert("not_planned_table".to_string(), not_planned_table);
    ctx.insert("not_planned_docs".to_string(), not_planned_docs);
    ctx.insert("behavioral_table".to_string(), behavioral_table);
    ctx.insert("behavioral_docs".to_string(), behavioral_docs);
    ctx.insert("investigate_table".to_string(), investigate_table);
    ctx.insert("adp_only_table".to_string(), adp_only_table);
    ctx.insert("adp_only_docs".to_string(), adp_only_docs);
    ctx.insert("transparent_table".to_string(), transparent_table);
    ctx.insert("issue_references".to_string(), issue_refs);

    let mut tt = TinyTemplate::new();
    tt.set_default_formatter(&tinytemplate::format_unescaped);
    tt.add_template("doc", &template_src)
        .unwrap_or_else(|e| panic!("template parse error: {}", e));

    let rendered = tt
        .render("doc", &ctx)
        .unwrap_or_else(|e| panic!("template render error: {}", e));

    let docs_dir = out_dir.join("docs");
    std::fs::create_dir_all(&docs_dir).unwrap();
    let out_path = docs_dir.join("dogstatsd.md");
    std::fs::write(&out_path, rendered).unwrap_or_else(|e| panic!("cannot write {}: {}", out_path.display(), e));
}
