use std::path::PathBuf;

use crate::types::RpcGeneratorFunction;

pub struct Config {
    pub in_file: PathBuf,
    pub out_file: PathBuf,
    pub single_module: bool,
    pub import_search_path: Vec<PathBuf>,
    pub no_output: bool,
    pub error_cycle: bool,
    pub headers: bool,
    pub dont_use_cow: bool,
    pub custom_struct_derive: Vec<String>,
    pub custom_repr: Option<String>,
    pub custom_rpc_generator: RpcGeneratorFunction,
    pub custom_includes: Vec<String>,
    pub owned: bool,
    pub nostd: bool,
    pub hashbrown: bool,
    pub gen_info: bool,
    pub add_deprecated_fields: bool,
    pub generate_getters: bool,
}
