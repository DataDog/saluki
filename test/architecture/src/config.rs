use crate::helpers::*;

const RAW_MAP_REASON: &str = "\
    These symbols belong to the raw-map config layer (saluki-config-tools). \
    This crate must use typed config structs from `saluki-component-config` or \
    `agent-data-plane-config` instead. Remove the offending references and \
    replace them with the corresponding typed config field.";

const RAW_MAP_METHODS_REASON: &str = "\
    These are raw-map access methods on GenericConfiguration/ConfigurationLoader. \
    Do not call them directly; route config reads through the typed config API in \
    `agent-data-plane-config-system` instead.";

const RAW_MAP_METHODS_REASON_TEST_EXEMPT: &str = "\
    These are raw-map access methods on GenericConfiguration/ConfigurationLoader. \
    Production code must route config reads through the typed config API in \
    `agent-data-plane-config-system` instead. Test code (#[cfg(test)]) is exempt.";

const REEXPORT_REASON: &str = "\
    Raw-map types must not be publicly re-exported from config-system. Consumers \
    should depend on typed config structs, not raw-map internals. Remove the \
    `pub use` or make it `pub(crate)`.";

const RETURN_REASON: &str = "\
    Public functions and methods in config-system must not return raw-map types \
    in their signature. Return a typed config struct instead, or make the item \
    `pub(crate)`.";

// ---------------------------------------------------------------------------
// Edge guards (dependency graph)
// ---------------------------------------------------------------------------

// bin/agent-data-plane must not depend on saluki-config-tools
// TODO: unignore when binary no longer depends on saluki-config-tools
#[test]
#[ignore = "binary still depends on saluki-config-tools"]
fn binary_must_not_depend_on_config_tools() {
    assert_no_dependency("agent-data-plane", "saluki-config-tools");
}

// bin/agent-data-plane must not depend on datadog-agent-config
// TODO: unignore when binary no longer depends on datadog-agent-config
#[test]
#[ignore = "binary still depends on datadog-agent-config"]
fn binary_must_not_depend_on_datadog_agent_config() {
    assert_no_dependency("agent-data-plane", "datadog-agent-config");
}

// saluki-components must not depend on agent-data-plane-config
#[test]
fn components_must_not_depend_on_adp_config() {
    assert_no_dependency("saluki-components", "agent-data-plane-config");
}

// saluki-components must not depend on datadog-agent-config
// TODO: unignore when components no longer depend on datadog-agent-config
#[test]
#[ignore = "components still depend on datadog-agent-config"]
fn components_must_not_depend_on_datadog_agent_config() {
    assert_no_dependency("saluki-components", "datadog-agent-config");
}

// saluki-components must not depend on saluki-config-tools
// TODO: unignore when components no longer depend on saluki-config-tools
#[test]
#[ignore = "components still depend on saluki-config-tools"]
fn components_must_not_depend_on_config_tools() {
    assert_no_dependency("saluki-components", "saluki-config-tools");
}

// saluki-component-config workspace normal deps allowlist
#[test]
fn leaf_config_deps_allowlist() {
    assert_workspace_normal_deps_subset(
        "saluki-component-config",
        &["saluki-context", "stringtheory"],
        "leaf_config_deps_allowlist",
    );
}

// agent-data-plane-config workspace normal deps allowlist
#[test]
fn adp_config_deps_allowlist() {
    assert_workspace_normal_deps_subset(
        "agent-data-plane-config",
        &["saluki-component-config", "saluki-io"],
        "adp_config_deps_allowlist",
    );
}

// agent-data-plane-config-system must not depend on saluki-components
#[test]
fn config_system_must_not_depend_on_components() {
    assert_no_dependency("agent-data-plane-config-system", "saluki-components");
}

// datadog-agent-config-overlay-model must be build-dep-only
#[test]
fn overlay_model_build_dep_only() {
    assert_build_dep_only_across_workspace(
        "datadog-agent-config-overlay-model",
        "This crate is a code-generation input only. A normal or dev dep would \
         expose generated overlay types as runtime artifacts to downstream crates.",
    );
}

// only agent-data-plane-config-system may normal-dep on saluki-config-tools
// TODO: unignore when disallowed crates no longer depend on saluki-config-tools
#[test]
#[ignore = "multiple crates still depend on saluki-config-tools"]
fn config_tools_sole_normal_dep_holder() {
    assert_sole_normal_dep_holder("saluki-config-tools", "agent-data-plane-config-system");
}

// ---------------------------------------------------------------------------
// Re-export guards (config-system public surface)
// ---------------------------------------------------------------------------

const RAW_MAP_TYPES: &[&str] = &["GenericConfiguration", "ConfigurationLoader"];

// config-system must not pub use re-export raw-map types
#[test]
fn config_system_no_pub_reexport_raw_map() {
    assert_no_pub_reexport("lib/agent-data-plane-config-system", RAW_MAP_TYPES, REEXPORT_REASON);
}

// config-system pub fns/methods must not return raw-map types
// TODO: unignore when `ConfigurationSystem::raw_map` is removed
#[test]
#[ignore = "config-system still exposes raw_map for un-flipped components"]
fn config_system_no_pub_fn_returning_raw_map() {
    assert_no_pub_fn_returning("lib/agent-data-plane-config-system", RAW_MAP_TYPES, RETURN_REASON);
}

// ---------------------------------------------------------------------------
// Symbol guards (defense-in-depth)
// ---------------------------------------------------------------------------

// binary must not name GenericConfiguration
// TODO: unignore when binary no longer uses GenericConfiguration
#[test]
#[ignore = "binary still references GenericConfiguration"]
fn binary_no_generic_configuration() {
    assert_no_symbol_in_crate("bin/agent-data-plane", &["GenericConfiguration"], RAW_MAP_REASON);
}

// binary must not name DatadogRemapper
// TODO: unignore when binary no longer uses DatadogRemapper
#[test]
#[ignore = "binary still references DatadogRemapper"]
fn binary_no_datadog_remapper() {
    assert_no_symbol_in_crate("bin/agent-data-plane", &["DatadogRemapper"], RAW_MAP_REASON);
}

// binary must not name KEY_ALIASES
// TODO: unignore when binary no longer uses KEY_ALIASES
#[test]
#[ignore = "binary still references KEY_ALIASES"]
fn binary_no_key_aliases() {
    assert_no_symbol_in_crate("bin/agent-data-plane", &["KEY_ALIASES"], RAW_MAP_REASON);
}

// ---------------------------------------------------------------------------
// Raw-map method bans
// ---------------------------------------------------------------------------

const RAW_MAP_METHODS: &[&str] = &[
    "try_get_typed",
    "as_typed",
    "watch_for_updates",
    "subscribe_for_updates",
];

// binary must not use raw-map methods
// TODO: unignore when binary no longer calls raw-map methods
#[test]
#[ignore = "binary still calls raw-map methods"]
fn binary_no_raw_map_methods() {
    assert_no_symbol_in_crate("bin/agent-data-plane", RAW_MAP_METHODS, RAW_MAP_METHODS_REASON);
}

// nothing outside config-system, config-tools, and tests may use raw-map methods
// TODO: unignore when raw-map method calls are confined to allowed crates
#[test]
#[ignore = "disallowed crates still call raw-map methods"]
fn raw_map_methods_confined_to_config_system() {
    let graph = package_graph();
    let skip = [
        "agent-data-plane-config-system",
        "architecture",
        "datadog-agent-config-testing",
        "saluki-config-tools",
    ];

    let mut all_violations: Vec<String> = Vec::new();
    for member in graph.workspace().iter() {
        if skip.contains(&member.name()) {
            continue;
        }
        let crate_path = member
            .source()
            .workspace_path()
            .expect("workspace member must have a path");
        let rel_path = crate_path.as_str();
        let hits = count_symbols_in_crate_excluding_tests(rel_path, RAW_MAP_METHODS);
        if hits > 0 {
            all_violations.push(format!("  {} ({} hit(s))", rel_path, hits));
        }
    }

    assert!(
        all_violations.is_empty(),
        "\n\
         ARCHITECTURE VIOLATION: raw-map methods {:?} found outside \
         `agent-data-plane-config-system` and `saluki-config-tools` (non-test code).\n\
         \n\
         Offending crates:\n{}\n\
         \n\
         {}\n",
        RAW_MAP_METHODS,
        all_violations.join("\n"),
        RAW_MAP_METHODS_REASON_TEST_EXEMPT,
    );
}
