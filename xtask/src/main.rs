use std::env;

mod gen_config_schema;

fn main() {
    let task = env::args().nth(1);
    match task.as_deref() {
        Some("gen-config-schema") => gen_config_schema::run(),
        _ => {
            eprintln!("Usage: cargo xtask <task>");
            eprintln!();
            eprintln!("Tasks:");
            eprintln!("  gen-config-schema   Regenerate config registry schema from vendored YAML");
            std::process::exit(1);
        }
    }
}
