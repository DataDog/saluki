//! Syntax-level scan of Rust sources for names.

use std::{
    collections::BTreeSet,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use proc_macro2::{Ident, TokenStream, TokenTree};
use snafu::{ResultExt as _, Snafu};
use syn::{
    ext::IdentExt as _,
    visit::{self, Visit},
    Item, UseTree, Visibility,
};

/// An error reading or parsing sources.
#[derive(Debug, Snafu)]
#[snafu(context(suffix(Snafu)))]
pub enum Error {
    /// A directory or file couldn't be read.
    #[snafu(display("failed to read {}: {source}", path.display()))]
    Read {
        /// The path being read.
        path: PathBuf,
        /// The underlying error.
        source: io::Error,
    },

    /// A file couldn't be parsed as Rust.
    #[snafu(display("failed to parse {}:{line}: {message}", path.display()))]
    Parse {
        /// The file.
        path: PathBuf,
        /// The 1-based line of the error.
        line: usize,
        /// The parse error message.
        message: String,
    },

    /// The scanned directories hold no `.rs` file.
    #[snafu(display("no `.rs` files under {}", display_paths(dirs)))]
    NoSources {
        /// The scanned directories.
        dirs: Vec<PathBuf>,
    },

    /// The crate root given for alias derivation isn't among the scanned files.
    #[snafu(display("crate root {} isn't a scanned file", path.display()))]
    MissingCrateRoot {
        /// The crate root.
        path: PathBuf,
    },

    /// A glob import of a scanned name binds names that alias derivation can't see.
    #[snafu(display(
        "{}:{line}: glob import of a guarded name; import the names explicitly",
        path.display()
    ))]
    GlobImport {
        /// The file.
        path: PathBuf,
        /// The 1-based line of the glob.
        line: usize,
    },
}

fn display_paths(paths: &[PathBuf]) -> String {
    let paths: Vec<_> = paths.iter().map(|path| path.display().to_string()).collect();
    paths.join(", ")
}

/// The syntax containing a matched identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Form {
    /// A path segment, with or without a leading `::`.
    Path,
    /// A name in a `use` tree, including the target of an `as` rename.
    Use,
    /// The crate or rename of an `extern crate` item.
    ExternCrate,
    /// An identifier token in a macro body or attribute argument.
    Token,
}

impl fmt::Display for Form {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Path => "path",
            Self::Use => "use",
            Self::ExternCrate => "extern crate",
            Self::Token => "token",
        })
    }
}

/// A matched identifier and its source location.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Mention {
    /// The file.
    pub path: PathBuf,
    /// The 1-based line.
    pub line: usize,
    /// The 1-based column.
    pub column: usize,
    /// The name.
    pub name: String,
    /// The syntax that names it.
    pub form: Form,
}

impl fmt::Display for Mention {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}: {} `{}`",
            self.path.display(),
            self.line,
            self.column,
            self.form,
            self.name
        )
    }
}

struct SourceFile {
    path: PathBuf,
    syntax: syn::File,
}

impl SourceFile {
    fn parse(path: PathBuf, text: &str) -> Result<Self, Error> {
        match syn::parse_file(text) {
            Ok(syntax) => Ok(Self { path, syntax }),
            Err(error) => ParseSnafu {
                path,
                line: error.span().start().line,
                message: error.to_string(),
            }
            .fail(),
        }
    }

    fn mentions(&self, names: &BTreeSet<String>) -> Vec<Mention> {
        let mut finder = MentionFinder::new(&self.path, names);
        finder.visit_file(&self.syntax);
        finder.found
    }

    fn module_mentions(&self, root: &Path, modules: &BTreeSet<String>) -> Vec<Mention> {
        let mut finder = ModuleFinder {
            path: &self.path,
            module: module_of(root, &self.path),
            modules,
            found: Vec::new(),
        };
        finder.visit_file(&self.syntax);
        finder.found
    }

    /// Adds cross-file aliases to `out`, following private alias chains within this file.
    /// [`SourceTree::aliases`] defines which bindings count across files.
    fn exported_aliases(
        &self, crate_root: bool, known: &BTreeSet<String>, out: &mut BTreeSet<String>,
    ) -> Result<(), Error> {
        let mut scope = known.clone();
        loop {
            let mut bindings = Bindings {
                path: &self.path,
                names: &scope,
                exported: BTreeSet::new(),
                private: BTreeSet::new(),
            };
            bindings.items(&self.syntax.items, crate_root)?;
            let Bindings { exported, private, .. } = bindings;
            let before = scope.len();
            scope.extend(exported.iter().chain(&private).cloned());
            if scope.len() == before {
                out.extend(exported);
                return Ok(());
            }
        }
    }
}

/// The parsed `.rs` files under a set of directories.
pub struct SourceTree {
    files: Vec<SourceFile>,
}

impl SourceTree {
    /// Reads and parses every `.rs` file under `dirs`, recursively.
    ///
    /// # Errors
    ///
    /// Returns an error if a directory or file can't be read, a file doesn't parse, or `dirs` hold no `.rs` file.
    pub fn read(dirs: &[PathBuf]) -> Result<Self, Error> {
        let mut paths = Vec::new();
        for dir in dirs {
            collect_rust_files(dir, &mut paths)?;
        }
        if paths.is_empty() {
            return NoSourcesSnafu { dirs: dirs.to_vec() }.fail();
        }
        let files = paths
            .into_iter()
            .map(|path| {
                let text = fs::read_to_string(&path).context(ReadSnafu { path: &path })?;
                SourceFile::parse(path, &text)
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { files })
    }

    /// Returns every place in these files that names one of `names`.
    ///
    /// Comments, doc text, and string literals never count.
    pub fn mentions(&self, names: &BTreeSet<String>) -> Vec<Mention> {
        self.files.iter().flat_map(|file| file.mentions(names)).collect()
    }

    /// Returns every path in these files that names a top-level module in `modules` of the crate whose sources are
    /// under `root`.
    ///
    /// Only paths that start at `crate`, `self` or `super` count, and `self` and `super` resolve from the file's
    /// place under `root` and its inline modules. Unlike [`SourceTree::mentions`], a local variable or a field that
    /// shares a module's name never matches. Paths in `use` items, types, expressions and macro tokens count.
    pub fn module_mentions(&self, root: &Path, modules: &BTreeSet<String>) -> Vec<Mention> {
        self.files
            .iter()
            .flat_map(|file| file.module_mentions(root, modules))
            .collect()
    }

    /// Collects aliases of `names` from this tree.
    ///
    /// An alias binds a name through an item mentioning a guarded name or another alias. At file and inline-module
    /// scope, the scan includes:
    ///
    /// - `use`, `type`, and `extern crate` items with visibility other than private or `pub(self)`
    /// - private bindings of those kinds at the top level of `crate_root`, accessible to descendant modules
    /// - `macro_rules!` definitions, regardless of visibility
    ///
    /// Private bindings elsewhere contribute only to same-file alias chains. Bindings inside blocks or macro
    /// invocations aren't collected. Matches ignore Rust scope, so unrelated names can match.
    ///
    /// # Errors
    ///
    /// Returns an error if `crate_root` isn't scanned or an included `use` item glob-imports a path containing a
    /// guarded name or alias. Glob-bound names can't be enumerated from the import syntax.
    pub fn aliases(&self, crate_root: &Path, names: &BTreeSet<String>) -> Result<BTreeSet<String>, Error> {
        if !self.files.iter().any(|file| file.path == crate_root) {
            return MissingCrateRootSnafu { path: crate_root }.fail();
        }
        let mut known = names.clone();
        loop {
            let mut bound = BTreeSet::new();
            for file in &self.files {
                file.exported_aliases(file.path == crate_root, &known, &mut bound)?;
            }
            let before = known.len();
            known.extend(bound);
            if known.len() == before {
                break;
            }
        }
        Ok(known.difference(names).cloned().collect())
    }
}

fn collect_rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), Error> {
    let mut entries = fs::read_dir(dir)
        .context(ReadSnafu { path: dir })?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .context(ReadSnafu { path: dir })?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            collect_rust_files(&path, out)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

struct MentionFinder<'a> {
    path: &'a Path,
    names: &'a BTreeSet<String>,
    found: Vec<Mention>,
}

impl<'a> MentionFinder<'a> {
    fn new(path: &'a Path, names: &'a BTreeSet<String>) -> Self {
        Self {
            path,
            names,
            found: Vec::new(),
        }
    }

    fn check(&mut self, ident: &Ident, form: Form) {
        let name = ident.unraw().to_string();
        if self.names.contains(&name) {
            let start = ident.span().start();
            self.found.push(Mention {
                path: self.path.to_path_buf(),
                line: start.line,
                column: start.column + 1,
                name,
                form,
            });
        }
    }

    fn tokens(&mut self, tokens: &TokenStream) {
        for token in tokens.clone() {
            match token {
                TokenTree::Ident(ident) => self.check(&ident, Form::Token),
                TokenTree::Group(group) => self.tokens(&group.stream()),
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
        }
    }
}

impl<'ast> Visit<'ast> for MentionFinder<'_> {
    fn visit_item_extern_crate(&mut self, i: &'ast syn::ItemExternCrate) {
        self.check(&i.ident, Form::ExternCrate);
        if let Some((_, rename)) = &i.rename {
            self.check(rename, Form::ExternCrate);
        }
        visit::visit_item_extern_crate(self, i);
    }

    fn visit_path_segment(&mut self, i: &'ast syn::PathSegment) {
        self.check(&i.ident, Form::Path);
        visit::visit_path_segment(self, i);
    }

    // Macro bodies, attribute arguments, and unparsed syntax.
    fn visit_token_stream(&mut self, i: &'ast TokenStream) {
        self.tokens(i);
    }

    fn visit_use_name(&mut self, i: &'ast syn::UseName) {
        self.check(&i.ident, Form::Use);
    }

    fn visit_use_path(&mut self, i: &'ast syn::UsePath) {
        self.check(&i.ident, Form::Use);
        visit::visit_use_path(self, i);
    }

    fn visit_use_rename(&mut self, i: &'ast syn::UseRename) {
        self.check(&i.ident, Form::Use);
        self.check(&i.rename, Form::Use);
    }
}

/// The module path of a source file under `root`: empty for the crate root, the directory for `mod.rs`.
fn module_of(root: &Path, path: &Path) -> Vec<String> {
    let relative = path.strip_prefix(root).unwrap_or(path);
    let mut module: Vec<String> = relative
        .iter()
        .map(|component| component.to_string_lossy().into_owned())
        .collect();
    match module.pop().as_deref() {
        Some("main.rs" | "lib.rs") if module.is_empty() => {}
        Some("mod.rs") | None => {}
        Some(file) => module.push(file.trim_end_matches(".rs").to_string()),
    }
    module
}

struct ModuleFinder<'a> {
    path: &'a Path,
    module: Vec<String>,
    modules: &'a BTreeSet<String>,
    found: Vec<Mention>,
}

impl ModuleFinder<'_> {
    /// Resolves a path that starts at `crate`, `self` or `super`, and records it if its top-level module is guarded.
    fn check(&mut self, segments: &[Ident]) {
        let Some((first, rest)) = segments.split_first() else {
            return;
        };
        let mut base = match first.to_string().as_str() {
            "crate" => Vec::new(),
            "self" => self.module.clone(),
            "super" => match self.module.split_last() {
                Some((_, parent)) => parent.to_vec(),
                None => return,
            },
            _ => return,
        };
        let mut reported = first;
        for segment in rest {
            match segment.to_string().as_str() {
                "super" => {
                    if base.pop().is_none() {
                        return;
                    }
                }
                "self" => {}
                _ => {
                    if base.is_empty() {
                        base.push(segment.unraw().to_string());
                        reported = segment;
                    }
                    break;
                }
            }
        }
        let Some(top) = base.first().filter(|top| self.modules.contains(*top)) else {
            return;
        };
        let start = reported.span().start();
        self.found.push(Mention {
            path: self.path.to_path_buf(),
            line: start.line,
            column: start.column + 1,
            name: top.clone(),
            form: Form::Path,
        });
    }

    fn use_tree(&mut self, tree: &UseTree, prefix: &mut Vec<Ident>) {
        match tree {
            UseTree::Path(path) => {
                prefix.push(path.ident.clone());
                self.use_tree(&path.tree, prefix);
                prefix.pop();
            }
            UseTree::Name(name) => self.use_leaf(prefix, &name.ident),
            UseTree::Rename(rename) => self.use_leaf(prefix, &rename.ident),
            UseTree::Glob(_) => self.check(prefix),
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.use_tree(tree, prefix);
                }
            }
        }
    }

    fn use_leaf(&mut self, prefix: &mut Vec<Ident>, ident: &Ident) {
        prefix.push(ident.clone());
        self.check(prefix);
        prefix.pop();
    }

    /// Checks every `a::b::c` run of identifiers in a token stream.
    fn tokens(&mut self, tokens: &TokenStream) {
        let tokens: Vec<TokenTree> = tokens.clone().into_iter().collect();
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                TokenTree::Ident(ident) => {
                    let mut segments = vec![ident.clone()];
                    while let (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b)), Some(TokenTree::Ident(next))) =
                        (tokens.get(i + 1), tokens.get(i + 2), tokens.get(i + 3))
                    {
                        if a.as_char() != ':' || b.as_char() != ':' {
                            break;
                        }
                        segments.push(next.clone());
                        i += 3;
                    }
                    self.check(&segments);
                }
                TokenTree::Group(group) => self.tokens(&group.stream()),
                TokenTree::Punct(_) | TokenTree::Literal(_) => {}
            }
            i += 1;
        }
    }
}

impl<'ast> Visit<'ast> for ModuleFinder<'_> {
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        if i.content.is_some() {
            self.module.push(i.ident.unraw().to_string());
            visit::visit_item_mod(self, i);
            self.module.pop();
        }
    }

    fn visit_item_use(&mut self, i: &'ast syn::ItemUse) {
        self.use_tree(&i.tree, &mut Vec::new());
    }

    fn visit_path(&mut self, i: &'ast syn::Path) {
        let segments: Vec<Ident> = i.segments.iter().map(|segment| segment.ident.clone()).collect();
        self.check(&segments);
        visit::visit_path(self, i);
    }

    // Macro bodies, attribute arguments, and unparsed syntax.
    fn visit_token_stream(&mut self, i: &'ast TokenStream) {
        self.tokens(i);
    }
}

/// Alias bindings split by whether the scan propagates them across files.
struct Bindings<'a> {
    path: &'a Path,
    names: &'a BTreeSet<String>,
    exported: BTreeSet<String>,
    private: BTreeSet<String>,
}

impl Bindings<'_> {
    fn items(&mut self, items: &[Item], export_private: bool) -> Result<(), Error> {
        for item in items {
            match item {
                Item::Use(item) => {
                    let exported = export_private || is_visible(&item.vis);
                    self.use_tree(&item.tree, None, exported)?;
                }
                Item::Type(item) => {
                    let mut finder = MentionFinder::new(self.path, self.names);
                    finder.visit_item_type(item);
                    if !finder.found.is_empty() {
                        self.bind(&item.ident, export_private || is_visible(&item.vis));
                    }
                }
                Item::ExternCrate(item) => {
                    if self.names_ident(&item.ident) {
                        let ident = item.rename.as_ref().map_or(&item.ident, |(_, rename)| rename);
                        self.bind(ident, export_private || is_visible(&item.vis));
                    }
                }
                Item::Macro(item) => {
                    if let Some(ident) = &item.ident {
                        let mut finder = MentionFinder::new(self.path, self.names);
                        finder.visit_macro(&item.mac);
                        if !finder.found.is_empty() {
                            self.bind(ident, true);
                        }
                    }
                }
                Item::Mod(item) => {
                    if let Some((_, items)) = &item.content {
                        self.items(items, false)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Collects bindings imported through guarded paths. `prefix` holds the preceding segment (for `self` imports)
    /// and whether any segment so far names a guarded identifier.
    fn use_tree(&mut self, tree: &UseTree, prefix: Option<(&Ident, bool)>, exported: bool) -> Result<(), Error> {
        let named = prefix.is_some_and(|(_, named)| named);
        match tree {
            UseTree::Path(path) => {
                let named = named || self.names_ident(&path.ident);
                self.use_tree(&path.tree, Some((&path.ident, named)), exported)?;
            }
            UseTree::Name(name) => {
                if named || self.names_ident(&name.ident) {
                    match prefix {
                        Some((parent, _)) if name.ident == "self" => self.bind(parent, exported),
                        _ => self.bind(&name.ident, exported),
                    }
                }
            }
            UseTree::Rename(rename) => {
                if (named || self.names_ident(&rename.ident)) && rename.rename != "_" {
                    self.bind(&rename.rename, exported);
                }
            }
            UseTree::Glob(glob) => {
                if named && exported {
                    return GlobImportSnafu {
                        path: self.path,
                        line: glob.star_token.spans[0].start().line,
                    }
                    .fail();
                }
            }
            UseTree::Group(group) => {
                for tree in &group.items {
                    self.use_tree(tree, prefix, exported)?;
                }
            }
        }
        Ok(())
    }

    fn names_ident(&self, ident: &Ident) -> bool {
        self.names.contains(&ident.unraw().to_string())
    }

    fn bind(&mut self, ident: &Ident, exported: bool) {
        let name = ident.unraw().to_string();
        if exported {
            self.exported.insert(name);
        } else {
            self.private.insert(name);
        }
    }
}

/// Whether the scan treats visibility as non-private: neither inherited nor `pub(self)`.
fn is_visible(vis: &Visibility) -> bool {
    match vis {
        Visibility::Public(_) => true,
        Visibility::Restricted(restricted) => !restricted.path.is_ident("self"),
        Visibility::Inherited => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn file(path: &str, text: &str) -> SourceFile {
        SourceFile::parse(PathBuf::from(path), text).unwrap_or_else(|error| panic!("{error}"))
    }

    fn found(text: &str) -> Vec<(usize, Form, String)> {
        file("lib.rs", text)
            .mentions(&names(&["guarded", "Alias"]))
            .into_iter()
            .map(|mention| (mention.line, mention.form, mention.name))
            .collect()
    }

    #[test]
    fn mentions_match_every_naming_form() {
        let cases = [
            ("rename", "use guarded::Thing as Other;", Form::Use, "guarded"),
            ("rename target", "use other::Thing as Alias;", Form::Use, "Alias"),
            ("leading `::`", "fn f() { ::guarded::run(); }", Form::Path, "guarded"),
            ("glob", "use guarded::*;", Form::Use, "guarded"),
            ("group", "use other::{a, guarded::b};", Form::Use, "guarded"),
            ("group leaf", "use other::{a, Alias};", Form::Use, "Alias"),
            (
                "type position",
                "struct S { field: Vec<guarded::T> }",
                Form::Path,
                "guarded",
            ),
            ("alias", "fn f(value: Alias) {}", Form::Path, "Alias"),
            ("raw identifier", "fn f(value: r#Alias) {}", Form::Path, "Alias"),
            (
                "non-root segment",
                "type T = crate::config::Alias;",
                Form::Path,
                "Alias",
            ),
            (
                "macro token",
                "fn f() { println!(\"{}\", guarded::VALUE); }",
                Form::Token,
                "guarded",
            ),
            (
                "macro_rules body",
                "macro_rules! m { () => { Alias::new() } }",
                Form::Token,
                "Alias",
            ),
            (
                "attribute token",
                "#[derive(guarded::Derive)] struct S;",
                Form::Token,
                "guarded",
            ),
            (
                "extern crate",
                "extern crate guarded as other;",
                Form::ExternCrate,
                "guarded",
            ),
            (
                "extern crate rename",
                "extern crate other as Alias;",
                Form::ExternCrate,
                "Alias",
            ),
            (
                "test code",
                "#[cfg(test)] mod tests { use guarded::X; }",
                Form::Use,
                "guarded",
            ),
        ];
        for (case, text, form, name) in cases {
            assert_eq!(found(text), [(1, form, name.to_string())], "{case}: {text}");
        }
    }

    #[test]
    fn mentions_ignore_comments_doc_text_strings_and_longer_names() {
        let text = r#"
            //! Module docs name guarded::Thing.
            // A comment names guarded::Thing.
            /* A block comment names Alias. */
            /// Item docs name [`guarded::Thing`].
            #[serde(with = "guarded::serde")]
            #[doc = "Alias"]
            struct S {
                field: guarded_other::Thing,
            }

            fn f() -> &'static str {
                println!("guarded::Thing");
                "Alias"
            }
        "#;
        assert_eq!(found(text), []);
    }

    #[test]
    fn mentions_report_line_and_column() {
        let mentions = file("dir/lib.rs", "fn f() {}\n\nfn g(value: Alias) {}\n").mentions(&names(&["Alias"]));
        let lines: Vec<_> = mentions.iter().map(Mention::to_string).collect();
        assert_eq!(lines, ["dir/lib.rs:3:13: path `Alias`"]);
    }

    #[test]
    fn module_mentions_resolve_paths_from_the_file_and_ignore_other_names() {
        let modules = names(&["guarded"]);
        let cases = [
            ("src/a.rs", "use crate::guarded::X;", 1),
            ("src/a.rs", "use crate::{other, guarded::{X, Y}};", 2),
            ("src/a.rs", "fn f() { crate::guarded::run(); }", 1),
            ("src/a.rs", "fn f() { assert!(crate::guarded::ok()); }", 1),
            ("src/a.rs", "use super::guarded;", 1),
            ("src/a/b.rs", "use super::guarded;", 0),
            ("src/a/b.rs", "use super::super::guarded;", 1),
            ("src/a/mod.rs", "use super::guarded::X;", 1),
            ("src/a.rs", "mod tests { use super::super::guarded::X; }", 1),
            ("src/guarded/a.rs", "use super::Thing;", 1),
            ("src/a.rs", "fn f(guarded: u8) -> u8 { let x = guarded; x }", 0),
            ("src/a.rs", "use other::guarded::X;", 0),
        ];
        for (path, text, expected) in cases {
            let found = file(path, text).module_mentions(Path::new("src"), &modules);
            assert_eq!(found.len(), expected, "{path}: {text}: {found:?}");
            assert!(found.iter().all(|mention| mention.name == "guarded"), "{found:?}");
        }
    }

    fn tree(files: &[(&str, &str)]) -> SourceTree {
        SourceTree {
            files: files.iter().map(|(path, text)| file(path, text)).collect(),
        }
    }

    fn aliases(files: &[(&str, &str)]) -> Result<BTreeSet<String>, Error> {
        tree(files).aliases(Path::new("main.rs"), &names(&["guarded"]))
    }

    #[test]
    fn aliases_include_root_imports_exported_items_and_their_file_private_sources() {
        let main = r#"
            extern crate guarded as renamed_crate;
            use guarded::{RootName, nested::Original as RootRename, nested::{self}};
            use guarded as _;
            type RootType = Vec<guarded::Thing>;
            use other::Unrelated;

            mod inline {
                use guarded::InlinePrivate;
                pub(crate) use guarded::InlineVisible;
                pub(crate) type InlineViaPrivate = InlinePrivate;
            }
        "#;
        let other = r#"
            pub(crate) type ViaPrivateChain = PrivateType;
            use guarded::OtherPrivate;
            pub(self) use guarded::OtherSelf;
            type PrivateType = Option<OtherSelf>;
            pub(crate) type ViaPrivate = Vec<OtherPrivate>;
            pub(crate) use guarded::OtherVisible;
            pub type OtherType = guarded::Thing;
            pub(crate) use crate::RootName as Transitive;
            pub(crate) type TransitiveType = Option<Transitive>;
            macro_rules! guarded_macro { () => { guarded::make() } }
            fn body() {
                pub use guarded::InBody;
            }
        "#;
        let unrelated = r#"
            pub(crate) type SameNameElsewhere = OtherPrivate;
        "#;

        let expected = names(&[
            "InlineViaPrivate",
            "InlineVisible",
            "OtherType",
            "OtherVisible",
            "RootName",
            "RootRename",
            "RootType",
            "Transitive",
            "TransitiveType",
            "ViaPrivate",
            "ViaPrivateChain",
            "guarded_macro",
            "nested",
            "renamed_crate",
        ]);
        assert_eq!(
            aliases(&[("main.rs", main), ("other.rs", other), ("unrelated.rs", unrelated)]).unwrap(),
            expected
        );
    }

    #[test]
    fn aliases_reject_glob_imports_of_guarded_paths() {
        let result = aliases(&[("main.rs", "use std::fmt;\nuse guarded::prelude::*;")]);
        assert!(
            matches!(&result, Err(Error::GlobImport { path, line: 2 }) if path == Path::new("main.rs")),
            "{:?}",
            result.map_err(|error| error.to_string())
        );

        let private_elsewhere = aliases(&[("main.rs", ""), ("other.rs", "use guarded::*;")]);
        assert_eq!(private_elsewhere.unwrap(), BTreeSet::new());
    }

    #[test]
    fn aliases_require_the_crate_root_among_the_files() {
        assert!(matches!(
            aliases(&[("lib.rs", "")]),
            Err(Error::MissingCrateRoot { path }) if path == Path::new("main.rs")
        ));
    }

    #[test]
    fn read_parses_nested_rust_files_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("a/b")).unwrap();
        fs::write(dir.path().join("a/b/deep.rs"), "use guarded::X;").unwrap();
        fs::write(dir.path().join("top.rs"), "fn f() {}").unwrap();
        fs::write(dir.path().join("README.md"), "use guarded::X; not Rust").unwrap();

        let tree = SourceTree::read(&[dir.path().to_path_buf()]).unwrap();
        let paths: Vec<_> = tree.files.iter().map(|file| file.path.clone()).collect();
        assert_eq!(paths, [dir.path().join("a/b/deep.rs"), dir.path().join("top.rs")]);
        let mentions: Vec<_> = tree
            .mentions(&names(&["guarded"]))
            .into_iter()
            .map(|m| m.path)
            .collect();
        assert_eq!(mentions, [dir.path().join("a/b/deep.rs")]);
    }

    #[test]
    fn read_fails_without_rust_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "").unwrap();

        assert!(matches!(
            SourceTree::read(&[dir.path().to_path_buf()]),
            Err(Error::NoSources { dirs }) if dirs == [dir.path().to_path_buf()]
        ));
        assert!(matches!(
            SourceTree::read(&[dir.path().join("missing")]),
            Err(Error::Read { path, .. }) if path == dir.path().join("missing")
        ));
    }

    #[test]
    fn read_fails_on_a_file_that_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("good.rs"), "fn f() {}").unwrap();
        fs::write(dir.path().join("bad.rs"), "fn f() {}\nfn g( {}\n").unwrap();

        match SourceTree::read(&[dir.path().to_path_buf()]) {
            Err(Error::Parse { path, line, .. }) => {
                assert_eq!(path, dir.path().join("bad.rs"));
                assert_eq!(line, 2);
            }
            Err(error) => panic!("unexpected error: {error}"),
            Ok(_) => panic!("a file that doesn't parse must fail the read"),
        }
    }
}
