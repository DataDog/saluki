//! Runtime selection of a schema `platform_default` value.
//!
//! Some schema settings declare a `platform_default` mapping (platform key -> value) instead of a single `default`.
//! The Agent resolves it when it starts, not when it is built (`getPlatformDefault` in `pkg/config/setup/config.go`):
//! `fargate` wins when running on ECS Fargate, then `container` when containerized, then the operating system, then
//! the `other` catch-all. The generated `platform_defaults` functions call [`resolve`] to pick the same key, so the
//! checked-in generated code stays independent of the host that produced it.

/// Returns the platform key whose value applies to this process, among the keys a setting declares.
///
/// Returns `""` when none applies (a mapping with no `other` key on an unlisted platform), in which case the setting
/// defaults to its type's zero value, as the Agent's `nil` default would.
pub(crate) fn resolve(declared: &[&'static str]) -> &'static str {
    resolve_with(declared, goos(), is_ecs_fargate(), is_containerized())
}

fn resolve_with(declared: &[&'static str], goos: &str, ecs_fargate: bool, containerized: bool) -> &'static str {
    let find = |key: &str| declared.iter().copied().find(|declared| *declared == key);
    let runtime = [(ecs_fargate, "fargate"), (containerized, "container")];
    runtime
        .into_iter()
        .filter(|(active, _)| *active)
        .find_map(|(_, key)| find(key))
        .or_else(|| find(goos))
        .or_else(|| find("other"))
        .unwrap_or("")
}

/// The operating system under the name Go's `runtime.GOOS` gives it, which is what platform keys use.
fn goos() -> &'static str {
    match std::env::consts::OS {
        "macos" => "darwin",
        os => os,
    }
}

/// Mirrors the Agent's `env.IsECSFargate`.
fn is_ecs_fargate() -> bool {
    std::env::var_os("ECS_FARGATE").is_some_and(|value| !value.is_empty())
        || std::env::var_os("AWS_EXECUTION_ENV").is_some_and(|value| value == "AWS_ECS_FARGATE")
}

/// Mirrors the Agent's `env.IsContainerized`.
fn is_containerized() -> bool {
    std::env::var_os("DOCKER_DD_AGENT").is_some_and(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::resolve_with;

    const ALL: &[&str] = &["aix", "container", "darwin", "fargate", "linux", "other", "windows"];

    #[test]
    fn the_operating_system_is_chosen_when_listed() {
        assert_eq!(resolve_with(ALL, "linux", false, false), "linux");
        assert_eq!(resolve_with(ALL, "darwin", false, false), "darwin");
    }

    #[test]
    fn fargate_takes_precedence_over_container_and_the_operating_system() {
        assert_eq!(resolve_with(ALL, "linux", true, true), "fargate");
    }

    #[test]
    fn container_takes_precedence_over_the_operating_system() {
        assert_eq!(resolve_with(ALL, "linux", false, true), "container");
    }

    #[test]
    fn a_runtime_key_applies_only_when_declared() {
        assert_eq!(resolve_with(&["linux", "other"], "linux", true, true), "linux");
    }

    #[test]
    fn an_unlisted_operating_system_falls_back_to_other() {
        assert_eq!(resolve_with(&["linux", "other"], "windows", false, false), "other");
    }

    #[test]
    fn no_key_applies_without_other() {
        assert_eq!(resolve_with(&["linux", "windows"], "freebsd", false, false), "");
    }

    #[test]
    fn the_host_operating_system_uses_go_naming() {
        assert_ne!(super::goos(), "macos");
    }
}
