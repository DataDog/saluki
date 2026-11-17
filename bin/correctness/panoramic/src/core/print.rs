//! The human format of a plan, for `panoramic plan --text`. No test pins it.

use std::fmt::{self, Write as _};

use super::plan::{CasePlan, Launch, PlannedParticipant};
use super::reference::Later;
use super::spec::{CommandSpec, Contents, Evidence, ExitChannel, OutputChannel};
use super::step::ProcedureNode;
use super::template::Template;
use super::wiring::{PlannedConnection, PlannedEndpoint};

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
        for (enclosure, resources) in &self.scope {
            writeln!(f, "{:<9} {:<20} {}", "scope", enclosure.to_string(), resources)?;
        }
        for (name, participant) in &self.participants {
            writeln!(f)?;
            participant.print(name, f)?;
        }
        writeln!(f)?;
        self.print_order(f)?;
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

impl CasePlan {
    fn print_order(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "order")?;
        let create: Vec<String> = self.create_order.iter().map(|level| level.join("  ")).collect();
        if create.len() > 1 {
            let levels = create
                .iter()
                .enumerate()
                .map(|(index, level)| format!("{:<5}{}", index + 1, level));
            field(f, "create", levels)?;
        } else {
            field(f, "create", create)?;
        }
        let mut lines = Vec::new();
        for (index, level) in self.start_order.iter().enumerate() {
            for (member, participant) in level.iter().enumerate() {
                let number = if member == 0 {
                    (index + 1).to_string()
                } else {
                    String::new()
                };
                let gates: Vec<String> = self
                    .gates
                    .get(participant)
                    .into_iter()
                    .flatten()
                    .map(|gate| format!("{} {}", gate.target, gate.evidence))
                    .collect();
                let mut line = format!("{:<5}{:<16}", number, participant);
                if !gates.is_empty() {
                    let _ = write!(line, "after {}", gates.join(", "));
                }
                lines.push(line.trim_end().to_string());
            }
        }
        field(f, "start", lines)
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
            .map(|(name, endpoint)| endpoint_line(name, endpoint));
        field(f, "endpoints", endpoints)?;
        let connect = self
            .connections
            .iter()
            .map(|(name, connection)| connection_line(name, connection));
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
        let later = self.later.iter().map(|(place, waits)| {
            let waits: Vec<String> = waits
                .iter()
                .map(|(participant, when)| match when {
                    Later::Created => format!("'{}' is created", participant),
                    Later::Started => format!("'{}' starts", participant),
                })
                .collect();
            format!("{} once {}", place, waits.join(" and "))
        });
        field(f, "filled", later)?;
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

fn endpoint_line(name: &str, endpoint: &PlannedEndpoint) -> String {
    let mut protocol = endpoint.protocol.as_str().to_string();
    if endpoint.tls {
        protocol.push_str(" tls");
    }
    let mut line = format!("{:<22}{:<16}{:<42}", name, protocol, endpoint.address.to_string());
    if endpoint.published {
        line.push_str("  published; host port chosen at start");
    }
    match (&endpoint.unwired, &endpoint.evidence) {
        (Some(_), _) => line.push_str("  not wired"),
        (None, Evidence::None) => line.push_str("  ready: none"),
        (None, evidence) => {
            let _ = write!(line, "  ready: {}", evidence);
        }
    }
    line
}

fn connection_line(name: &str, connection: &PlannedConnection) -> String {
    let mut line = format!(
        "{:<37} {}",
        format!("{} -> {}", name, connection.target),
        connection.url
    );
    if !connection.delivers.is_empty() {
        let _ = write!(line, "   delivers {}", connection.delivers.join(", "));
    }
    line
}

impl fmt::Display for Evidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Accepts => f.write_str("accepts a connection"),
            Self::Exists => f.write_str("exists"),
            Self::None => f.write_str("has started"),
            Self::Http { path } => write!(f, "answers 200 on {}", path),
            Self::Output { pattern, process } => write!(f, "has '{}' in the output of {}", pattern, process),
        }
    }
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
