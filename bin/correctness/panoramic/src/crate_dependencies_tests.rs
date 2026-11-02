//! Checks the workspace dependency boundaries between panoramic crates.

use std::{collections::BTreeMap, process::Command};

use serde_json::{json, Value};

const PERMITTED: &[(&str, &[&str])] = &[
    (
        "panoramic",
        &["panoramic-core", "panoramic-participants", "panoramic-steps"],
    ),
    ("panoramic-core", &[]),
    ("panoramic-courier", &[]),
    ("panoramic-participants", &["panoramic-core"]),
    ("panoramic-steps", &["panoramic-core"]),
];

/// Keeps core and steps independent of product definitions by checking panoramic dependency directions.
/// Third-party dependencies are outside this check.
fn violations(metadata: &Value) -> Vec<String> {
    let mut packages: BTreeMap<_, _> = metadata["packages"]
        .as_array()
        .expect("metadata must list workspace packages")
        .iter()
        .map(|package| {
            (
                package["name"].as_str().expect("package must have a name"),
                Some(package),
            )
        })
        .collect();
    for (name, _) in PERMITTED {
        packages.entry(name).or_insert(None);
    }

    let is_panoramic = |name: &str| name == "panoramic" || name.starts_with("panoramic-");
    let mut problems = Vec::new();
    for (name, package) in packages {
        let Some(package) = package else {
            problems.push(format!("{name} is listed in PERMITTED but missing from the workspace"));
            continue;
        };
        let permitted = PERMITTED.iter().find(|(allowed_name, _)| *allowed_name == name);
        if is_panoramic(name) && permitted.is_none() {
            problems.push(format!("{name} is missing from PERMITTED"));
        }
        let allowed = permitted.map_or(&[][..], |(_, dependencies)| *dependencies);
        let mut dependencies: Vec<_> = package["dependencies"]
            .as_array()
            .expect("package must list dependencies")
            .iter()
            .map(|dependency| {
                (
                    dependency["name"]
                        .as_str()
                        .expect("dependency must have a package name"),
                    dependency["kind"].as_str().unwrap_or("normal"),
                    dependency["target"].as_str().unwrap_or(""),
                )
            })
            .collect();
        dependencies.sort_unstable();
        for (dependency, kind, _) in dependencies {
            if !is_panoramic(dependency) {
                continue;
            }
            if !is_panoramic(name) {
                problems.push(format!(
                    "{name} has a {kind} dependency on {dependency}; only panoramic workspace crates may depend on panoramic crates"
                ));
            } else if !allowed.contains(&dependency) {
                problems.push(format!(
                    "{name} has a {kind} dependency on {dependency}; permitted panoramic dependencies: {allowed:?}"
                ));
            }
        }
    }
    problems
}

fn workspace_metadata() -> Value {
    json!({
        "packages": [
            {
                "name": "panoramic",
                "dependencies": [
                    {"name": "panoramic-core", "kind": null},
                    {"name": "panoramic-participants", "kind": null},
                    {"name": "panoramic-steps", "kind": null},
                    {"name": "panoramic-steps", "kind": "dev"}
                ]
            },
            {
                "name": "panoramic-core",
                "dependencies": [{"name": "airlock", "kind": null}]
            },
            {
                "name": "panoramic-participants",
                "dependencies": [{"name": "panoramic-core", "kind": null}]
            },
            {
                "name": "panoramic-steps",
                "dependencies": [{"name": "panoramic-core", "kind": null}]
            },
            {
                "name": "panoramic-courier",
                "dependencies": []
            }
        ]
    })
}

#[test]
fn panoramic_crates_depend_only_in_permitted_directions() {
    let output = Command::new(env!("CARGO"))
        .args([
            "metadata",
            "--format-version",
            "1",
            "--no-deps",
            "--offline",
            "--manifest-path",
            concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"),
        ])
        .output()
        .expect("cargo metadata must run");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata = serde_json::from_slice(&output.stdout).expect("cargo metadata must return JSON");
    let problems = violations(&metadata);
    assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn a_forbidden_panoramic_dependency_is_reported_for_every_dependency_kind() {
    let mut metadata = workspace_metadata();
    for (index, dependency, kind, target, rename) in [
        (
            3,
            "panoramic-participants",
            "dev",
            Some("cfg(windows)"),
            Some("products"),
        ),
        (1, "panoramic-steps", "normal", None, None),
        (2, "panoramic-steps", "build", None, None),
    ] {
        metadata["packages"][index]["dependencies"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "name": dependency,
                "kind": if kind == "normal" { None } else { Some(kind) },
                "target": target,
                "rename": rename,
            }));
    }
    metadata["packages"].as_array_mut().unwrap().reverse();
    assert_eq!(
        violations(&metadata),
        [
            "panoramic-core has a normal dependency on panoramic-steps; permitted panoramic dependencies: []",
            "panoramic-participants has a build dependency on panoramic-steps; permitted panoramic dependencies: [\"panoramic-core\"]",
            "panoramic-steps has a dev dependency on panoramic-participants; permitted panoramic dependencies: [\"panoramic-core\"]",
        ]
    );
}

#[test]
fn a_workspace_crate_outside_panoramic_may_not_depend_on_panoramic_crates() {
    let mut metadata = workspace_metadata();
    metadata["packages"].as_array_mut().unwrap().push(json!({
        "name": "airlock",
        "dependencies": [{"name": "panoramic-core", "kind": null}]
    }));
    assert_eq!(
        violations(&metadata),
        ["airlock has a normal dependency on panoramic-core; only panoramic workspace crates may depend on panoramic crates"]
    );
}

#[test]
fn every_panoramic_workspace_crate_must_have_a_permitted_entry() {
    let mut metadata = workspace_metadata();
    metadata["packages"][3] = json!({"name": "panoramic-extra", "dependencies": []});
    assert_eq!(
        violations(&metadata),
        [
            "panoramic-extra is missing from PERMITTED",
            "panoramic-steps is listed in PERMITTED but missing from the workspace",
        ]
    );
}
