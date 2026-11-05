//! Checks source boundaries that crate dependencies cannot enforce.
//!
//! Product knowledge belongs in participant types. Steps use handles and leave realizations, host mechanisms,
//! and product paths to their providers. These scans cover fixtures, comments, and embedded assets too.

use std::{fs, path::Path};

enum Term {
    Text(&'static str),
    Ident(&'static str),
    CfgIdent(&'static str),
    Number(&'static str),
}

impl Term {
    fn find_in(&self, line: &str) -> Option<&'static str> {
        match self {
            Self::Text(term) if line.to_ascii_lowercase().contains(term) => Some(term),
            Self::Ident(term) if matches_token(line, term, |byte| byte.is_ascii_alphanumeric() || byte == b'_') => {
                Some(term)
            }
            Self::CfgIdent(term) if line.contains("cfg") => Self::Ident(term).find_in(line),
            Self::Number(term) if matches_token(line, term, |byte| byte.is_ascii_digit()) => Some(term),
            _ => None,
        }
    }
}

/// Identifier boundaries use ASCII letters, digits, and underscores; number boundaries use digits alone.
fn matches_token(line: &str, term: &str, is_token_byte: fn(u8) -> bool) -> bool {
    let bytes = line.as_bytes();
    line.match_indices(term).any(|(start, _)| {
        let end = start + term.len();
        (start == 0 || !is_token_byte(bytes[start - 1])) && (end == bytes.len() || !is_token_byte(bytes[end]))
    })
}

struct Boundary {
    crate_dir: &'static str,
    terms: &'static [Term],
    exact_exemptions: &'static [(&'static str, &'static str)],
    guidance: &'static str,
}

const CORE: Boundary = Boundary {
    crate_dir: "core",
    terms: &[
        Term::Text("agent"),
        Term::Text("adp"),
        Term::Text("datadog"),
        Term::Text("dogstatsd"),
        Term::Text("intake"),
        Term::Text("millstone"),
        Term::Text("otlp"),
        Term::Text("s6"),
        Term::Number("2049"),
        Term::Number("55001"),
    ],
    exact_exemptions: &[],
    guidance: "Product knowledge belongs in panoramic-participants.",
};

const STEPS: Boundary = Boundary {
    crate_dir: "steps",
    terms: &[
        Term::Text("docker"),
        Term::Text("pwsh"),
        Term::Text("powershell"),
        Term::Text("kube"),
        Term::Text("container"),
        Term::Text("host process"),
        Term::Text("host_process"),
        Term::Text("hostprocess"),
        Term::Text("/opt/"),
        Term::Text("/etc/"),
        Term::Text("/usr/"),
        Term::Text("/var/"),
        Term::Text("c:\\"),
        Term::CfgIdent("windows"),
        Term::CfgIdent("unix"),
        Term::CfgIdent("target_os"),
        Term::CfgIdent("target_family"),
        Term::Ident("Os"),
        Term::Ident("Realization"),
        Term::Ident("RealizationKind"),
        Term::Ident("Pod"),
        Term::Ident("pod"),
    ],
    exact_exemptions: STEPS_METRIC_PREFIX_EXEMPTIONS,
    guidance:
        "Steps reach participants through handles and must not name realizations, host mechanisms or product paths.",
};

const STEPS_METRIC_PREFIX_EXEMPTIONS: &[(&str, &str)] = &[
    (
        "steps/src/compare/analysis/metrics/mod.rs",
        "|| name.starts_with(\"docker.\")",
    ),
    (
        "steps/src/compare/analysis/metrics/mod.rs",
        "|| name.starts_with(\"container.\")",
    ),
];

impl Boundary {
    fn violations(&self, files: &[(String, String)]) -> Vec<String> {
        let mut files: Vec<_> = files.iter().collect();
        files.sort_unstable_by_key(|(path, _)| path);
        let mut problems = Vec::new();
        for (path, text) in files {
            for (index, line) in text.lines().enumerate() {
                if self.exact_exemptions.contains(&(path.as_str(), line.trim())) {
                    continue;
                }
                let terms: Vec<_> = self.terms.iter().filter_map(|term| term.find_in(line)).collect();
                if !terms.is_empty() {
                    problems.push(format!("{path}:{}: names {}", index + 1, terms.join(", ")));
                }
            }
        }
        problems
    }

    #[track_caller]
    fn assert_sources(&self) {
        let files = source_files(Path::new(env!("CARGO_MANIFEST_DIR")), self.crate_dir)
            .unwrap_or_else(|error| panic!("{error}\n{}", self.guidance));
        assert!(
            !files.is_empty(),
            "{}: no source files found\n{}",
            self.crate_dir,
            self.guidance
        );
        let problems = self.violations(&files);
        assert!(problems.is_empty(), "{}\n{}", problems.join("\n"), self.guidance);
    }
}

/// Reads every file in the crate recursively, excluding target directories.
fn source_files(root: &Path, crate_dir: &str) -> Result<Vec<(String, String)>, String> {
    let lib = format!("{crate_dir}/src/lib.rs");
    if !root.join(&lib).is_file() {
        return Err(format!(
            "{lib}: required crate source file is missing or not a regular file"
        ));
    }

    let manifest = format!("{crate_dir}/Cargo.toml");
    fs::symlink_metadata(root.join(&manifest)).map_err(|error| format!("{manifest}: {error}"))?;

    let mut pending = vec![crate_dir.to_string()];
    let mut files = Vec::new();
    while let Some(relative) = pending.pop() {
        let path = root.join(&relative);
        let metadata = fs::symlink_metadata(&path).map_err(|error| format!("{relative}: {error}"))?;
        if metadata.is_dir() {
            if path.ends_with("target") {
                continue;
            }
            let entries = fs::read_dir(&path).map_err(|error| format!("{relative}: {error}"))?;
            for entry in entries {
                let entry = entry.map_err(|error| format!("{relative}: {error}"))?;
                let name = entry.file_name();
                let name = name
                    .to_str()
                    .ok_or_else(|| format!("{relative}: non-UTF-8 file name"))?;
                pending.push(format!("{relative}/{name}"));
            }
        } else if metadata.is_file() {
            let text = fs::read_to_string(&path).map_err(|error| format!("{relative}: {error}"))?;
            files.push((relative, text));
        } else {
            return Err(format!("{relative}: expected a regular file or directory"));
        }
    }
    files.sort_unstable_by(|left, right| left.0.cmp(&right.0));
    Ok(files)
}

#[test]
fn panoramic_core_source_names_no_product() {
    CORE.assert_sources();
}

#[test]
fn panoramic_steps_source_names_no_realization_host_mechanism_or_product_path() {
    STEPS.assert_sources();
}

#[test]
fn a_core_term_is_reported_case_insensitively_with_its_file_and_line() {
    for newline in ["\n", "\r\n"] {
        let text = ["AgentAdp", "DATADOG", "ADP agent adp"].join(newline);
        assert_eq!(
            CORE.violations(&[("core/src/example.rs".into(), text)]),
            [
                "core/src/example.rs:1: names agent, adp",
                "core/src/example.rs:2: names datadog",
                "core/src/example.rs:3: names agent, adp",
            ]
        );
    }
}

#[test]
fn a_comment_on_a_core_line_does_not_exempt_its_terms() {
    assert_eq!(
        CORE.violations(&[(
            "core/src/example.rs".into(),
            "adp // required by an external API".into(),
        )]),
        ["core/src/example.rs:1: names adp"]
    );
}

#[test]
fn numbers_and_identifiers_match_only_whole_tokens() {
    assert_eq!(
        CORE.violations(&[(
            "core/src/ports.rs".into(),
            "2049\n12049 20490\n55001\n155001 550010\nport2049_name\n".into(),
        )]),
        [
            "core/src/ports.rs:1: names 2049",
            "core/src/ports.rs:3: names 55001",
            "core/src/ports.rs:5: names 2049",
        ]
    );
    assert_eq!(
        STEPS.violations(&[(
            "steps/src/example.rs".into(),
            concat!(
                "Os::Linux\n",
                "use x::Os;\n",
                "Cost OsString my_Os Os_name 1Os Os1 os OS\n",
                "slice.windows(2)\n",
                "#[cfg(windows)]\n",
                "cfg!(windows)\n",
                "#[cfg(not(windows))]\n",
                "RealizationKind Realization Pod pod\n",
                "RealizationKindExtra RealizationExtra Pods tripod pod_name\n",
                "éOsé\n",
            )
            .into(),
        )]),
        [
            "steps/src/example.rs:1: names Os",
            "steps/src/example.rs:2: names Os",
            "steps/src/example.rs:5: names windows",
            "steps/src/example.rs:6: names windows",
            "steps/src/example.rs:7: names windows",
            "steps/src/example.rs:8: names Realization, RealizationKind, Pod, pod",
            "steps/src/example.rs:10: names Os",
        ]
    );
}

#[test]
fn host_os_cfg_identifiers_match_within_nested_conditions_and_macros() {
    // Literal parenthesized names miss nested cfg conditions; match identifiers across the line.
    for (line, expected) in [
        ("#[cfg(any(unix, windows))]", "windows, unix"),
        ("#[cfg(all(windows, test))]", "windows"),
        ("cfg!(windows)", "windows"),
        ("cfg! ( unix )", "unix"),
        ("#[cfg(not(windows))]", "windows"),
        ("#[cfg(target_os = \"linux\")]", "target_os"),
        ("#[cfg_attr(target_family = \"unix\", test)]", "unix, target_family"),
        ("#[cfg(feature = \"x\")] fn windows() {}", "windows"),
        ("#[cfg(windows_extra, my_unix, target_os2, my_target_family)]", ""),
        ("#[cfg(my_windows, unix2, my_target_os, target_family_extra)]", ""),
        ("slice.windows(2); let unix = target_os + target_family;", ""),
    ] {
        let expected = if expected.is_empty() {
            Vec::new()
        } else {
            vec![format!("steps/src/example.rs:1: names {expected}")]
        };
        assert_eq!(
            STEPS.violations(&[("steps/src/example.rs".into(), line.into())]),
            expected,
            "{line}"
        );
    }
}

#[test]
fn violations_are_sorted_by_path_then_line() {
    assert_eq!(
        CORE.violations(&[
            ("core/src/z.rs".into(), "adp\n".into()),
            ("core/src/a.rs".into(), "\nagent\n\n\n\n\n\n\n\n\nintake\n".into()),
        ]),
        [
            "core/src/a.rs:2: names agent",
            "core/src/a.rs:11: names intake",
            "core/src/z.rs:1: names adp",
        ]
    );
}

#[test]
fn source_files_include_the_manifest_and_nested_assets_in_path_order() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("core/src/assets/nested")).unwrap();
    fs::write(dir.path().join("core/Cargo.toml"), "agent").unwrap();
    fs::write(dir.path().join("core/src/lib.rs"), "").unwrap();
    fs::write(dir.path().join("core/src/assets/nested/start.sh"), "adp").unwrap();
    fs::write(dir.path().join("core/src/assets/settings"), "otlp").unwrap();

    let files = source_files(dir.path(), "core").unwrap();
    assert_eq!(
        files,
        [
            ("core/Cargo.toml".into(), "agent".into()),
            ("core/src/assets/nested/start.sh".into(), "adp".into()),
            ("core/src/assets/settings".into(), "otlp".into()),
            ("core/src/lib.rs".into(), "".into()),
        ]
    );
    assert_eq!(
        CORE.violations(&files),
        [
            "core/Cargo.toml:1: names agent",
            "core/src/assets/nested/start.sh:1: names adp",
            "core/src/assets/settings:1: names otlp",
        ]
    );
}

#[test]
fn source_files_scan_the_whole_crate_except_target_directories() {
    // Starting at the crate root catches product and host names in every Cargo target and asset.
    for (relative, scanned) in [
        ("tests/nested/example.rs", true),
        ("benches/example.rs", true),
        ("examples/example.rs", true),
        ("build.rs", true),
        ("assets/settings", true),
        (".hidden/settings", true),
        ("target/debug/generated.rs", false),
        ("examples/target/generated.rs", false),
    ] {
        for (boundary, term) in [(&CORE, "agent"), (&STEPS, "docker")] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join(boundary.crate_dir);
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(root.join("Cargo.toml"), "").unwrap();
            fs::write(root.join("src/lib.rs"), "").unwrap();
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, term).unwrap();

            let files = source_files(dir.path(), boundary.crate_dir).unwrap();
            let expected = if scanned {
                vec![format!("{}/{relative}:1: names {term}", boundary.crate_dir)]
            } else {
                Vec::new()
            };
            assert_eq!(
                boundary.violations(&files),
                expected,
                "{}: {relative}",
                boundary.crate_dir
            );
            assert_eq!(files.len(), if scanned { 3 } else { 2 }, "{relative}");

            if !scanned {
                fs::write(&path, [0xff]).unwrap();
                assert_eq!(source_files(dir.path(), boundary.crate_dir).unwrap(), files);
            }
        }
    }
}

#[test]
fn a_crate_directory_without_sources_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let expected = "core/src/lib.rs: required crate source file is missing or not a regular file";
    assert_eq!(source_files(dir.path(), "core").unwrap_err(), expected);
    fs::create_dir_all(dir.path().join("core/src")).unwrap();
    fs::write(dir.path().join("core/Cargo.toml"), "").unwrap();
    assert_eq!(source_files(dir.path(), "core").unwrap_err(), expected);
}

#[test]
fn a_missing_manifest_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("core/src")).unwrap();
    fs::write(dir.path().join("core/src/lib.rs"), "").unwrap();
    let error = source_files(dir.path(), "core").unwrap_err();
    assert!(error.starts_with("core/Cargo.toml: "), "{error}");
}

#[test]
fn a_non_utf8_source_file_is_an_error() {
    for relative in ["src/assets/data.bin", "tests/data.bin", "build.rs"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("core");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, [0xff]).unwrap();
        let error = source_files(dir.path(), "core").unwrap_err();
        assert!(error.starts_with(&format!("core/{relative}: ")), "{error}");
    }
}

#[cfg(unix)]
#[test]
fn symlinks_and_special_files_are_errors() {
    use std::os::unix::{fs::symlink, net::UnixListener};

    for relative in ["src/linked", "tests", "target"] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("core");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("Cargo.toml"), "").unwrap();
        fs::write(root.join("src/lib.rs"), "").unwrap();
        let path = root.join(relative);
        let expected = format!("core/{relative}: expected a regular file or directory");

        symlink(root.join("src"), &path).unwrap();
        assert_eq!(source_files(dir.path(), "core").unwrap_err(), expected);
        fs::remove_file(&path).unwrap();

        let _socket = UnixListener::bind(&path).unwrap();
        assert_eq!(source_files(dir.path(), "core").unwrap_err(), expected);
    }
}

#[test]
fn only_exact_metric_filter_prefix_lines_are_exempt() {
    let path = "steps/src/compare/analysis/metrics/mod.rs";
    for line in [
        r#"|| name.starts_with("docker.")"#,
        r#"|| name.starts_with("container.")"#,
    ] {
        assert!(STEPS.violations(&[(path.into(), line.into())]).is_empty());
        assert!(!STEPS
            .violations(&[("steps/src/other.rs".into(), line.into())])
            .is_empty());
        for changed in [
            format!("{line} // container"),
            line.replace("name", "path"),
            line.replace("docker.", "docker/").replace("container.", "container/"),
        ] {
            assert!(!STEPS.violations(&[(path.into(), changed)]).is_empty());
        }
    }
}

#[test]
fn metric_prefix_exemptions_require_both_the_exact_path_and_line() {
    for &(path, line) in STEPS_METRIC_PREFIX_EXEMPTIONS {
        assert!(STEPS.violations(&[(path.into(), line.into())]).is_empty());
        assert_eq!(STEPS.violations(&[("steps/src/other.rs".into(), line.into())]).len(), 1);
        assert_eq!(
            STEPS
                .violations(&[(path.into(), format!("{line} // another use"))])
                .len(),
            1
        );
    }
}
