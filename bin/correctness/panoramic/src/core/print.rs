//! The human format of a plan, for `panoramic plan --text`. No test pins it.

use std::fmt::{self, Write as _};

use super::plan::{CasePlan, Launch, PlannedParticipant};
use super::spec::{CommandSpec, Contents, EndpointSpec, Evidence, ExitChannel, OutputChannel};
use super::step::ProcedureNode;
use super::template::Template;

/// Width of the participant and field name columns.
const NAME_WIDTH: usize = 12;

impl fmt::Display for CasePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{:<9} {:<20} {}", "case", self.case.name, self.case.file.display())?;
        writeln!(
            f,
            "{:<9} {:<20} timeout {}",
            "subject",
            self.subject_os.to_string(),
            self.case.timeout.text
        )?;
        if let Some(description) = &self.case.description {
            writeln!(f, "{:<10}{}", "", description.trim())?;
        }
        for (name, participant) in &self.participants {
            writeln!(f)?;
            participant.print(name, f)?;
        }
        writeln!(f)?;
        writeln!(f, "procedure")?;
        if self.procedure.is_empty() {
            writeln!(f, "  (no steps)")?;
        }
        for (index, node) in self.procedure.iter().enumerate() {
            for (member, step) in node.steps().iter().enumerate() {
                let label = match node {
                    ProcedureNode::Step(_) => (index + 1).to_string(),
                    ProcedureNode::Parallel(_) => format!("{}.{}", index + 1, member + 1),
                };
                let mut line = format!("  {:<5}{}", label, step.name);
                for (key, value) in &step.fields.0 {
                    let _ = write!(line, "  {} {}", key, value);
                }
                writeln!(f, "{}", line)?;
            }
        }
        Ok(())
    }
}

impl PlannedParticipant {
    fn print(&self, name: &str, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let participant_type = self.participant_type.as_deref().unwrap_or("(no participant type)");
        let role = if self.subject { "subject" } else { "" };
        writeln!(
            f,
            "{:<9} {:<23} {:<9} {}",
            name, participant_type, role, self.realization.enclosure
        )?;
        let options: Vec<String> = self
            .options
            .iter()
            .map(|(option, value)| format!("{}={}", option, value))
            .collect();
        field(f, "options", options)?;

        match &self.launch {
            Launch::Image { image, command, args } => {
                field(f, "image", [image])?;
                let command = command
                    .as_ref()
                    .map_or_else(|| "(the image's own)".to_string(), |command| words(command));
                field(f, "command", [command])?;
                if let Some(args) = args {
                    field(f, "args", [words(args)])?;
                }
            }
            Launch::Programs(programs) => {
                let lines = programs.iter().map(|program| {
                    let mut line = format!("{:<10}{}", program.name, program.path);
                    if !program.args.is_empty() {
                        let _ = write!(line, " {}", words(&program.args));
                    }
                    for (key, value) in &program.env {
                        let _ = write!(line, "  {}={}", key, value);
                    }
                    line
                });
                field(f, "programs", lines)?;
            }
        }

        field(
            f,
            "env",
            self.env.iter().map(|(key, value)| format!("{}={}", key, value)),
        )?;
        let files = self.files.iter().map(|file| {
            let mut line = format!("{}  <- {}", file.destination, file.source);
            if matches!(file.contents, Contents::Template(_)) {
                line.push_str(" (template)");
            }
            if let Some(mode) = file.mode {
                let _ = write!(line, "  {:04o}", mode);
            }
            if let Some(name) = &file.name {
                let _ = write!(line, "  as {}", name);
            }
            line
        });
        field(f, "files", files)?;
        let mounts = self.mounts.iter().map(|mount| {
            let access = if mount.read_only { "  read-only" } else { "" };
            format!("{} -> {}{}", mount.host, mount.path, access)
        });
        field(f, "mounts", mounts)?;
        let endpoints = self
            .endpoints
            .iter()
            .map(|(name, endpoint)| endpoint_line(name, endpoint, self.ready.get(name)));
        field(f, "endpoints", endpoints)?;
        let connect = self
            .connect
            .iter()
            .map(|(name, target)| format!("{} -> {}", name, target));
        field(f, "connect", connect)?;
        let processes = self.processes.iter().map(|process| {
            let exit = match process.exit() {
                ExitChannel::Container => "container".to_string(),
                ExitChannel::Record { file } => format!("record {}", file),
            };
            let output = match process.output() {
                OutputChannel::Container => "container".to_string(),
                OutputChannel::File { file } => format!("file {}", file),
            };
            format!("{:<10}exit: {:<20}output: {}", process.name(), exit, output)
        });
        field(f, "processes", processes)?;
        field(
            f,
            "commands",
            self.commands.iter().map(|(name, command)| command_line(name, command)),
        )?;
        let collect = self.collect.iter().map(|file| {
            let keep = match file.keep {
                super::spec::Keep::Always => "always",
                super::spec::Keep::OnFailure => "on failure",
            };
            format!("{} -> {}  keep {}", file.path, file.name, keep)
        });
        field(f, "collect", collect)?;
        if !self.operations.is_empty() {
            let names: Vec<&str> = self.operations.keys().copied().collect();
            field(f, "operations", [names.join("  ")])?;
        }
        Ok(())
    }
}

/// Writes a field's lines under its name, or nothing for an empty field.
fn field<I>(f: &mut fmt::Formatter<'_>, name: &str, lines: I) -> fmt::Result
where
    I: IntoIterator,
    I::Item: fmt::Display,
{
    for (index, line) in lines.into_iter().enumerate() {
        let label = if index == 0 { name } else { "" };
        writeln!(f, "  {:<NAME_WIDTH$}{}", label, line)?;
    }
    Ok(())
}

fn words(templates: &[Template]) -> String {
    let words: Vec<&str> = templates.iter().map(Template::text).collect();
    words.join(" ")
}

fn endpoint_line(name: &str, endpoint: &EndpointSpec, ready: Option<&Evidence>) -> String {
    let mut protocol = endpoint.protocol.as_str().to_string();
    if endpoint.tls {
        protocol.push_str(" tls");
    }
    // TODO: print the planned address once the harness owns addresses.
    let mut line = format!("{:<20}{:<16}", name, protocol);
    if let Some(ready) = ready {
        let evidence = match ready {
            Evidence::Accepts => "accepts".to_string(),
            Evidence::Exists => "socket exists".to_string(),
            Evidence::None => "none".to_string(),
            Evidence::Http { path } => format!("answers 200 on {}", path),
            Evidence::Output { pattern, process } => format!("{} writes '{}'", process, pattern),
        };
        let _ = write!(line, "ready: {}", evidence);
    }
    line.trim_end().to_string()
}

fn command_line(name: &str, command: &CommandSpec) -> String {
    let mut line = format!("{:<10}", name);
    if let Some(program) = &command.program {
        let _ = write!(line, "program {} ", program);
    }
    line.push_str(&words(&command.argv));
    for (key, value) in &command.env {
        let _ = write!(line, "  {}={}", key, value);
    }
    line
}
