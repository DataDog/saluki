pub mod errors;
mod keywords;
mod parser;
mod scc;
pub mod types;

use errors::{Error, Result};
use std::path::{Path, PathBuf};
use types::Config;

/// A builder for [Config]
///
/// # Example build.rs
///
/// ```rust,no_run
/// use piecemeal_build::{types::FileDescriptor, ConfigBuilder};
/// use std::path::{Path, PathBuf};
/// use walkdir::WalkDir;
///
/// # fn main() {
/// let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("protos");
/// let include_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("protos");
/// // Re-run this build.rs if the protos dir changes (i.e. a new file is added)
/// println!("cargo:rerun-if-changed={}", include_dir.to_str().unwrap());
///
/// // Find all *.proto files in the `in_dir` and add them to the list of files
/// let mut input_files = Vec::new();
/// let proto_ext = Some(Path::new("proto").as_os_str());
/// for entry in WalkDir::new(&include_dir) {
///     let path = entry.unwrap().into_path();
///     if path.extension() == proto_ext {
///         // Re-run this build.rs if any of the files in the protos dir change
///         println!("cargo:rerun-if-changed={}", path.to_str().unwrap());
///         input_files.push(path);
///     }
/// }
///
/// // Delete all old generated files before re-generating new ones
/// if out_dir.exists() {
///     std::fs::remove_dir_all(&out_dir).unwrap();
/// }
/// std::fs::DirBuilder::new().create(&out_dir).unwrap();
/// let config_builder = ConfigBuilder::new(&input_files, out_dir, &[include_dir]).unwrap();
/// FileDescriptor::run(&config_builder.build()).unwrap()
/// # }
/// ```
#[derive(Debug, Default)]
pub struct ConfigBuilder {
    in_files: Vec<PathBuf>,
    output_dir: PathBuf,
    include_paths: Vec<PathBuf>,
    single_module: bool,
    error_cycle: bool,
    headers: bool,
    dont_use_cow: bool,
    custom_struct_derive: Vec<String>,
    custom_repr: Option<String>,
    owned: bool,
    nostd: bool,
    hashbrown: bool,
    gen_info: bool,
    add_deprecated_fields: bool,
    generate_getters: bool,
}

impl ConfigBuilder {
    pub fn new<I, O, IP>(in_files: &[I], output_dir: O, include_paths: &[IP]) -> Result<ConfigBuilder>
    where
        I: AsRef<Path>,
        O: AsRef<Path>,
        IP: AsRef<Path>,
    {
        let in_files = in_files.iter().map(|f| f.as_ref().to_path_buf()).collect::<Vec<_>>();

        if in_files.is_empty() {
            return Err(Error::NoProto);
        }

        for f in &in_files {
            if !f.exists() {
                return Err(Error::InputFile(format!("{}", f.display())));
            }
        }

        let output_dir = output_dir.as_ref().to_path_buf();
        if !output_dir.is_dir() {
            std::fs::create_dir_all(&output_dir).map_err(Error::FailedToCreateOutputDirectory)?;
        }

        let mut include_paths = include_paths
            .iter()
            .map(|f| f.as_ref().to_path_buf())
            .collect::<Vec<_>>();

        let default = PathBuf::from(".");
        if include_paths.is_empty() || !include_paths.contains(&default) {
            include_paths.push(default);
        }

        Ok(ConfigBuilder {
            in_files,
            output_dir,
            include_paths,
            headers: true,
            ..Default::default()
        })
    }

    /// Omit generation of modules for each package when there is only one package
    pub fn single_module(mut self, val: bool) -> Self {
        self.single_module = val;
        self
    }

    /// Error out if recursive messages do not have optional fields
    pub fn error_cycle(mut self, val: bool) -> Self {
        self.error_cycle = val;
        self
    }

    /// Enable module comments and module attributes in generated file (default = true)
    pub fn headers(mut self, val: bool) -> Self {
        self.headers = val;
        self
    }

    /// Add custom values to `#[derive(...)]` at the beginning of every structure
    pub fn custom_struct_derive(mut self, val: Vec<String>) -> Self {
        self.custom_struct_derive = val;
        self
    }

    /// Add custom values to `#[repr(...)]` at the beginning of every structure
    pub fn custom_repr(mut self, val: Option<String>) -> Self {
        self.custom_repr = val;
        self
    }

    /// Use `Cow<_,_>` for Strings and Bytes
    pub fn dont_use_cow(mut self, val: bool) -> Self {
        self.dont_use_cow = val;
        self
    }

    /// Generate Owned structs when the proto struct has a lifetime
    pub fn owned(mut self, val: bool) -> Self {
        self.owned = val;
        self
    }

    /// Generate `#![no_std]` compliant code
    pub fn nostd(mut self, val: bool) -> Self {
        self.nostd = val;
        self
    }

    /// Use hashbrown as `HashMap` implementation instead of [std::collections::HashMap] or
    /// [alloc::collections::BTreeMap](https://doc.rust-lang.org/alloc/collections/btree_map/struct.BTreeMap.html)
    /// in a `no_std` environment
    pub fn hashbrown(mut self, val: bool) -> Self {
        self.hashbrown = val;
        self
    }

    /// Generate `MessageInfo` implementations
    pub fn gen_info(mut self, val: bool) -> Self {
        self.gen_info = val;
        self
    }

    /// Add deprecated fields and mark them as `#[deprecated]`
    pub fn add_deprecated_fields(mut self, val: bool) -> Self {
        self.add_deprecated_fields = val;
        self
    }

    /// Generate getters for Proto2 `optional` fields with custom default values
    pub fn generate_getters(mut self, val: bool) -> Self {
        self.generate_getters = val;
        self
    }

    /// Build [Config] from this `ConfigBuilder`
    pub fn build(self) -> Vec<Config> {
        self.in_files
            .into_iter()
            .map(|in_file| {
                let out_dir = self.output_dir.clone();

                Config {
                    in_file,
                    out_dir,
                    import_search_path: self.include_paths.clone(),
                    single_module: self.single_module,
                    error_cycle: self.error_cycle,
                    headers: self.headers,
                    dont_use_cow: self.dont_use_cow, //Change this to true to not use cow with ./generate.sh for v2 and v3 tests
                    custom_struct_derive: self.custom_struct_derive.clone(),
                    custom_repr: self.custom_repr.clone(),
                    custom_includes: Vec::new(),
                    owned: self.owned,
                    nostd: self.nostd,
                    hashbrown: self.hashbrown,
                    gen_info: self.gen_info,
                    add_deprecated_fields: self.add_deprecated_fields,
                    generate_getters: self.generate_getters,
                }
            })
            .collect()
    }
}
