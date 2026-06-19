//! A test suite for enforcing architectural invariants.
//!
//! As a measure of defense against architectural degradation, these tests assert invariants like:
//! - crate-a must not depend on crate-b
//! - crate-c must be a workspace leaf crate
//! - symbols X and Y must not appear in crate-z
//!
//! The enforcement mechanisms are not perfect. One can get around them by renaming a symbol, for
//! example, but they provide a fast signal to coding agents and well-meaning humans that might not
//! understand the intended architectural boundaries between systems.
//!
//! Renaming affected crates will require updating these tests, but the trade-off seems worth it
//! to prevent accidental leakage of internals. The goal is to provide stronger, deeper abstractions
//! that remain coherent over time.
#[cfg(test)]
mod config;

#[cfg(test)]
mod helpers {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::OnceLock;
    use std::{fmt, fs};

    use guppy::graph::{DependencyDirection, PackageGraph};
    use guppy::MetadataCommand;
    use syn::visit::Visit;
    use syn::{Attribute, ExprMethodCall, File, ImplItemFn, ItemFn, ItemMod, ItemUse, ReturnType, UseTree};

    pub fn workspace_root() -> &'static Path {
        static ROOT: OnceLock<PathBuf> = OnceLock::new();
        ROOT.get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            manifest_dir
                .parent()
                .expect("test/ dir")
                .parent()
                .expect("workspace root")
                .to_path_buf()
        })
    }

    pub(crate) fn package_graph() -> &'static PackageGraph {
        static GRAPH: OnceLock<PackageGraph> = OnceLock::new();
        GRAPH.get_or_init(|| {
            MetadataCommand::new()
                .manifest_path(workspace_root().join("Cargo.toml"))
                .build_graph()
                .expect("failed to build package graph from cargo metadata")
        })
    }

    // ---- guppy helpers ----

    pub(crate) fn assert_no_dependency(from: &str, to: &str) {
        let graph = package_graph();
        let from_id = graph
            .workspace()
            .member_by_name(from)
            .unwrap_or_else(|e| panic!("workspace member {from:?} not found: {e}"))
            .id();
        let to_id = graph
            .workspace()
            .member_by_name(to)
            .unwrap_or_else(|e| panic!("workspace member {to:?} not found: {e}"))
            .id();

        let depends = graph
            .query_forward([from_id])
            .expect("forward query")
            .resolve()
            .contains(to_id)
            .expect("resolve contains");

        assert!(
            !depends,
            "\n\
             ARCHITECTURE VIOLATION: `{from}` must not depend on `{to}` \
             (even transitively).\n\
             \n\
             To find the dependency chain, run:\n\
             \n\
             \x20   cargo tree -p {from} -i {to}\n\
             \n\
             Then remove or restructure the intermediate dependency.\n"
        );
    }

    pub(crate) fn assert_workspace_normal_deps_subset(pkg_name: &str, allowed: &[&str], test_fn_name: &str) {
        let graph = package_graph();
        let pkg = graph
            .workspace()
            .member_by_name(pkg_name)
            .unwrap_or_else(|e| panic!("workspace member {pkg_name:?} not found: {e}"));

        let allowed_set: HashSet<&str> = allowed.iter().copied().collect();
        let workspace_ids: HashSet<_> = graph.workspace().iter().map(|m| m.id().to_owned()).collect();

        let mut violations = Vec::new();
        for link in pkg.direct_links_directed(DependencyDirection::Forward) {
            if !workspace_ids.contains(link.to().id()) {
                continue;
            }
            let has_normal = link.normal().is_present();
            if has_normal && !allowed_set.contains(link.to().name()) {
                violations.push(link.to().name().to_string());
            }
        }

        assert!(
            violations.is_empty(),
            "\n\
             ARCHITECTURE VIOLATION: `{pkg_name}` has workspace normal deps outside \
             the allowlist.\n\
             Allowed: {allowed:?}\n\
             Unexpected: {violations:?}\n\
             \n\
             Remove the unexpected dep from `{pkg_name}`'s Cargo.toml. This crate's \
             dep set is intentionally constrained to stay thin and independently testable.\n\
             \n\
             If the new dep is genuinely required (rare), update the `allowed` slice in \
             `test/architecture/src/config.rs::{test_fn_name}` with team review.\n"
        );
    }

    pub(crate) fn assert_build_dep_only_across_workspace(dep_name: &str, reason: &str) {
        let graph = package_graph();
        let dep_id = graph
            .workspace()
            .member_by_name(dep_name)
            .unwrap_or_else(|e| panic!("workspace member {dep_name:?} not found: {e}"))
            .id();

        let mut violations = Vec::new();
        for member in graph.workspace().iter() {
            for link in member.direct_links_directed(DependencyDirection::Forward) {
                if link.to().id() != dep_id {
                    continue;
                }
                if link.normal().is_present() || link.dev().is_present() {
                    violations.push(member.name().to_string());
                }
            }
        }

        assert!(
            violations.is_empty(),
            "\n\
             ARCHITECTURE VIOLATION: `{dep_name}` must be a build-dependency only, \
             but these crates have a normal or dev dependency on it:\n\
             {violations:?}\n\
             \n\
             {reason}\n\
             \n\
             Move the dep to [build-dependencies] in each offending Cargo.toml.\n"
        );
    }

    pub(crate) fn assert_sole_normal_dep_holder(dep_name: &str, sole_holder: &str) {
        let graph = package_graph();
        let dep_id = graph
            .workspace()
            .member_by_name(dep_name)
            .unwrap_or_else(|e| panic!("workspace member {dep_name:?} not found: {e}"))
            .id();

        let mut holders = Vec::new();
        for member in graph.workspace().iter() {
            for link in member.direct_links_directed(DependencyDirection::Forward) {
                if link.to().id() != dep_id {
                    continue;
                }
                if link.normal().is_present() && member.name() != sole_holder {
                    holders.push(member.name().to_string());
                }
            }
        }

        assert!(
            holders.is_empty(),
            "\n\
             ARCHITECTURE VIOLATION: only `{sole_holder}` may have a normal dep on \
             `{dep_name}`, but these crates also do:\n\
             {holders:?}\n\
             \n\
             Move the dep to [dev-dependencies] or [build-dependencies], or remove it.\n"
        );
    }

    // ---- syn helpers ----

    struct SymbolHit {
        file: PathBuf,
        line: usize,
        col: usize,
    }

    impl fmt::Display for SymbolHit {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            let rel = self.file.strip_prefix(workspace_root()).unwrap_or(&self.file);
            write!(f, "  {}:{}:{}", rel.display(), self.line, self.col)
        }
    }

    fn format_hits(hits: &[SymbolHit]) -> String {
        hits.iter()
            .take(10)
            .map(|h| h.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn collect_rs_files(dir: &Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        collect_rs_files_inner(dir, &mut files);
        files
    }

    fn collect_rs_files_inner(dir: &Path, files: &mut Vec<PathBuf>) {
        let entries = fs::read_dir(dir).unwrap_or_else(|e| panic!("failed to read {}: {e}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_files_inner(&path, files);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }

    fn parse_file(path: &Path) -> File {
        let content = fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        syn::parse_file(&content).unwrap_or_else(|e| panic!("failed to parse {}: {e}", path.display()))
    }

    fn has_test_attr(attrs: &[Attribute]) -> bool {
        attrs.iter().any(|a| a.path().is_ident("test"))
    }

    fn has_cfg_test_attr(attrs: &[Attribute]) -> bool {
        attrs.iter().any(|a| {
            if !a.path().is_ident("cfg") {
                return false;
            }
            a.parse_args::<syn::Ident>()
                .map(|ident| ident == "test")
                .unwrap_or(false)
        })
    }

    struct SymbolScanner<'a> {
        symbols: &'a [&'a str],
        hits: Vec<SymbolHit>,
        file_path: PathBuf,
        skip_test_items: bool,
    }

    impl<'a, 'ast> Visit<'ast> for SymbolScanner<'a> {
        fn visit_item_fn(&mut self, node: &'ast ItemFn) {
            if self.skip_test_items && has_test_attr(&node.attrs) {
                return;
            }
            syn::visit::visit_item_fn(self, node);
        }

        fn visit_item_mod(&mut self, node: &'ast ItemMod) {
            if self.skip_test_items && has_cfg_test_attr(&node.attrs) {
                return;
            }
            syn::visit::visit_item_mod(self, node);
        }

        fn visit_path(&mut self, node: &'ast syn::Path) {
            for segment in &node.segments {
                let ident = segment.ident.to_string();
                if self.symbols.contains(&ident.as_str()) {
                    let span = segment.ident.span();
                    self.hits.push(SymbolHit {
                        file: self.file_path.clone(),
                        line: span.start().line,
                        col: span.start().column + 1,
                    });
                }
            }
            syn::visit::visit_path(self, node);
        }

        fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
            let ident = node.method.to_string();
            if self.symbols.contains(&ident.as_str()) {
                let span = node.method.span();
                self.hits.push(SymbolHit {
                    file: self.file_path.clone(),
                    line: span.start().line,
                    col: span.start().column + 1,
                });
            }
            syn::visit::visit_expr_method_call(self, node);
        }
    }

    fn scan_crate_for_symbols(crate_src_dir: &Path, symbols: &[&str], skip_test_items: bool) -> Vec<SymbolHit> {
        let mut all_hits = Vec::new();
        for path in collect_rs_files(crate_src_dir) {
            let file = parse_file(&path);
            let mut scanner = SymbolScanner {
                symbols,
                hits: Vec::new(),
                file_path: path,
                skip_test_items,
            };
            scanner.visit_file(&file);
            all_hits.extend(scanner.hits);
        }
        all_hits
    }

    pub(crate) fn count_symbols_in_crate_excluding_tests(crate_rel_path: &str, symbols: &[&str]) -> usize {
        let src_dir = workspace_root().join(crate_rel_path).join("src");
        scan_crate_for_symbols(&src_dir, symbols, true).len()
    }

    pub(crate) fn assert_no_symbol_in_crate(crate_rel_path: &str, symbols: &[&str], reason: &str) {
        let src_dir = workspace_root().join(crate_rel_path).join("src");
        let hits = scan_crate_for_symbols(&src_dir, symbols, false);
        assert!(
            hits.is_empty(),
            "\n\
             ARCHITECTURE VIOLATION: `{crate_rel_path}/src/` must not reference \
             {symbols:?}.\n\
             Found {} hit(s):\n{}\n\
             \n\
             {reason}\n",
            hits.len(),
            format_hits(&hits),
        );
    }

    struct PubReexportScanner<'a> {
        symbols: &'a [&'a str],
        hits: Vec<SymbolHit>,
        file_path: PathBuf,
    }

    fn use_tree_contains_symbol(tree: &UseTree, symbols: &[&str]) -> bool {
        match tree {
            UseTree::Path(p) => {
                if symbols.contains(&p.ident.to_string().as_str()) {
                    return true;
                }
                use_tree_contains_symbol(&p.tree, symbols)
            }
            UseTree::Name(n) => symbols.contains(&n.ident.to_string().as_str()),
            UseTree::Rename(r) => symbols.contains(&r.ident.to_string().as_str()),
            UseTree::Glob(_) => false,
            UseTree::Group(g) => g.items.iter().any(|t| use_tree_contains_symbol(t, symbols)),
        }
    }

    impl<'a, 'ast> Visit<'ast> for PubReexportScanner<'a> {
        fn visit_item_use(&mut self, node: &'ast ItemUse) {
            if matches!(node.vis, syn::Visibility::Public(_)) && use_tree_contains_symbol(&node.tree, self.symbols) {
                let span = node.use_token.span;
                self.hits.push(SymbolHit {
                    file: self.file_path.clone(),
                    line: span.start().line,
                    col: span.start().column + 1,
                });
            }
            syn::visit::visit_item_use(self, node);
        }
    }

    pub(crate) fn assert_no_pub_reexport(crate_rel_path: &str, symbols: &[&str], reason: &str) {
        let src_dir = workspace_root().join(crate_rel_path).join("src");
        let mut all_hits = Vec::new();
        for path in collect_rs_files(&src_dir) {
            let file = parse_file(&path);
            let mut scanner = PubReexportScanner {
                symbols,
                hits: Vec::new(),
                file_path: path,
            };
            scanner.visit_file(&file);
            all_hits.extend(scanner.hits);
        }
        assert!(
            all_hits.is_empty(),
            "\n\
             ARCHITECTURE VIOLATION: `{crate_rel_path}` must not `pub use` re-export \
             {symbols:?}.\n\
             Found {} hit(s):\n{}\n\
             \n\
             {reason}\n",
            all_hits.len(),
            format_hits(&all_hits),
        );
    }

    struct PubFnReturnScanner<'a> {
        symbols: &'a [&'a str],
        hits: Vec<SymbolHit>,
        file_path: PathBuf,
    }

    fn return_type_contains_symbol(ret: &ReturnType, symbols: &[&str]) -> bool {
        match ret {
            ReturnType::Default => false,
            ReturnType::Type(_, ty) => type_contains_symbol(ty, symbols),
        }
    }

    fn type_contains_symbol(ty: &syn::Type, symbols: &[&str]) -> bool {
        match ty {
            syn::Type::Path(tp) => tp.path.segments.iter().any(|seg| {
                if symbols.contains(&seg.ident.to_string().as_str()) {
                    return true;
                }
                if let syn::PathArguments::AngleBracketed(args) = &seg.arguments {
                    args.args.iter().any(|arg| {
                        if let syn::GenericArgument::Type(inner) = arg {
                            type_contains_symbol(inner, symbols)
                        } else {
                            false
                        }
                    })
                } else {
                    false
                }
            }),
            syn::Type::Reference(r) => type_contains_symbol(&r.elem, symbols),
            syn::Type::Tuple(t) => t.elems.iter().any(|e| type_contains_symbol(e, symbols)),
            syn::Type::Paren(p) => type_contains_symbol(&p.elem, symbols),
            _ => false,
        }
    }

    impl<'a, 'ast> Visit<'ast> for PubFnReturnScanner<'a> {
        fn visit_item_fn(&mut self, node: &'ast ItemFn) {
            if matches!(node.vis, syn::Visibility::Public(_))
                && return_type_contains_symbol(&node.sig.output, self.symbols)
            {
                let span = node.sig.ident.span();
                self.hits.push(SymbolHit {
                    file: self.file_path.clone(),
                    line: span.start().line,
                    col: span.start().column + 1,
                });
            }
            syn::visit::visit_item_fn(self, node);
        }

        fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
            if matches!(node.vis, syn::Visibility::Public(_))
                && return_type_contains_symbol(&node.sig.output, self.symbols)
            {
                let span = node.sig.ident.span();
                self.hits.push(SymbolHit {
                    file: self.file_path.clone(),
                    line: span.start().line,
                    col: span.start().column + 1,
                });
            }
            syn::visit::visit_impl_item_fn(self, node);
        }
    }

    pub(crate) fn assert_no_pub_fn_returning(crate_rel_path: &str, symbols: &[&str], reason: &str) {
        let src_dir = workspace_root().join(crate_rel_path).join("src");
        let mut all_hits = Vec::new();
        for path in collect_rs_files(&src_dir) {
            let file = parse_file(&path);
            let mut scanner = PubFnReturnScanner {
                symbols,
                hits: Vec::new(),
                file_path: path,
            };
            scanner.visit_file(&file);
            all_hits.extend(scanner.hits);
        }
        assert!(
            all_hits.is_empty(),
            "\n\
             ARCHITECTURE VIOLATION: `{crate_rel_path}` pub functions/methods must not return \
             {symbols:?}.\n\
             Found {} hit(s):\n{}\n\
             \n\
             {reason}\n",
            all_hits.len(),
            format_hits(&all_hits),
        );
    }
}
