//! Guards the help contract at the process boundary: `-h` and `--help` are interchangeable.
//!
//! The contents of that help, including the environment-variable inventory, are covered by the
//! unit tests in `src/cli.rs`.

use std::process::Command;

#[test]
fn short_and_long_help_flags_produce_identical_output() {
    for subcommand in [None, Some("run"), Some("list")] {
        let short = help_output(subcommand, "-h");
        let long = help_output(subcommand, "--help");

        assert!(!short.is_empty(), "help output should not be empty");
        assert_eq!(
            short,
            long,
            "`-h` and `--help` disagree for `panoramic {}`",
            subcommand.unwrap_or_default()
        );
    }
}

fn help_output(subcommand: Option<&str>, flag: &str) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_panoramic"));
    command.args(subcommand).arg(flag);

    let output = command.output().expect("panoramic should be runnable");
    assert!(
        output.status.success(),
        "`panoramic {} {}` exited with {:?}",
        subcommand.unwrap_or_default(),
        flag,
        output.status
    );

    String::from_utf8(output.stdout).expect("help output should be UTF-8")
}
