use std::io::Write;

use datadog_protos::agent::command::v1::{
    Command as RcpCommand, CommandParameter as RcpCommandParameter, ParameterType,
};
use prost_types::{value::Kind, Struct};
use saluki_error::{generic_error, GenericError};
use tracing::info;

/// Describes a remotely executable CLI command.
pub(crate) struct RemoteCommandDescriptor {
    /// The command path segment accepted by the CLI.
    pub(crate) name: &'static str,
    /// The command help text exposed through the remote-command protocol.
    pub(crate) helper: &'static str,
    /// The named parameters accepted by the command.
    pub(crate) parameters: &'static [RemoteParameterDescriptor],
}

impl RemoteCommandDescriptor {
    /// Converts the descriptor to remote-command protocol metadata.
    pub(crate) fn to_rcp_command(&self) -> RcpCommand {
        RcpCommand {
            name: self.name.to_string(),
            short_name: self.name.to_string(),
            helper: self.helper.to_string(),
            parameters: self
                .parameters
                .iter()
                .map(RemoteParameterDescriptor::to_rcp_parameter)
                .collect(),
            is_runnable: true,
            ..Default::default()
        }
    }
}

/// Describes a named parameter for a remotely executable CLI command.
pub(crate) struct RemoteParameterDescriptor {
    /// The parameter's long CLI option name, without the leading dashes.
    pub(crate) name: &'static str,
    /// The parameter's short CLI option name, without the leading dash.
    pub(crate) short_name: &'static str,
    /// The parameter help text exposed through the remote-command protocol.
    pub(crate) helper: &'static str,
    /// The parameter value type accepted through the remote-command protocol.
    pub(crate) argument_type: RemoteArgumentType,
    /// Whether the parameter must be supplied for the command to run.
    pub(crate) required: bool,
}

impl RemoteParameterDescriptor {
    /// Converts the descriptor to remote-command protocol metadata.
    fn to_rcp_parameter(&self) -> RcpCommandParameter {
        RcpCommandParameter {
            name: self.name.to_string(),
            short_name: self.short_name.to_string(),
            helper: self.helper.to_string(),
            r#type: self.argument_type.to_rcp_parameter_type() as i32,
            required: self.required,
            is_flag: true,
            is_persistent: false,
        }
    }
}

/// The remote-command protocol types currently supported by the CLI.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RemoteArgumentType {
    /// A UTF-8 string.
    String,
    /// A boolean value.
    Bool,
    /// An unsigned integer.
    Uint,
}

impl RemoteArgumentType {
    const fn to_rcp_parameter_type(self) -> ParameterType {
        match self {
            Self::String => ParameterType::TypeString,
            Self::Bool => ParameterType::TypeBool,
            Self::Uint => ParameterType::TypeUint,
        }
    }
}

/// Validates remote-command arguments and converts them to command-line arguments.
///
/// Arguments are ordered according to `descriptors`, rather than the unspecified map order of protobuf `Struct`
/// fields. Required options and defaults remain the responsibility of the CLI parser that consumes the result.
pub(crate) fn remote_command_argv(
    command_path: &[String], arguments: &Struct, descriptors: &[RemoteCommandDescriptor], command_group: &str,
) -> Result<Vec<String>, GenericError> {
    let [command] = command_path else {
        return Err(generic_error!(
            "expected exactly one {command_group} command path segment"
        ));
    };
    let descriptor = descriptors
        .iter()
        .find(|descriptor| descriptor.name == command)
        .ok_or_else(|| generic_error!("unknown {command_group} command `{command}`"))?;

    for name in arguments.fields.keys() {
        if !descriptor.parameters.iter().any(|parameter| parameter.name == name) {
            return Err(generic_error!(
                "unexpected argument `{name}` for {command_group} command `{command}`"
            ));
        }
    }

    let mut argv = vec![command.clone()];
    for parameter in descriptor.parameters {
        let Some(value) = arguments.fields.get(parameter.name) else {
            continue;
        };
        argv.push(format!("--{}", parameter.name));
        argv.push(remote_argument_value(
            parameter.name,
            value.kind.as_ref(),
            parameter.argument_type,
        )?);
    }

    Ok(argv)
}

fn remote_argument_value(
    name: &str, kind: Option<&Kind>, expected_type: RemoteArgumentType,
) -> Result<String, GenericError> {
    match (expected_type, kind) {
        (RemoteArgumentType::String, Some(Kind::StringValue(value))) => Ok(value.clone()),
        (RemoteArgumentType::Bool, Some(Kind::BoolValue(value))) => Ok(value.to_string()),
        (RemoteArgumentType::Uint, Some(Kind::NumberValue(value)))
            if value.is_finite() && *value >= 0.0 && value.fract() == 0.0 && *value < u64::MAX as f64 =>
        {
            Ok(format!("{value:.0}"))
        }
        (RemoteArgumentType::String, _) => Err(generic_error!("argument `{name}` must be a string")),
        (RemoteArgumentType::Bool, _) => Err(generic_error!("argument `{name}` must be a boolean")),
        (RemoteArgumentType::Uint, _) => Err(generic_error!("argument `{name}` must be an unsigned integer")),
    }
}

/// Receives a command's progress and report output.
pub(crate) trait CommandOutput: Send {
    /// Writes a progress update for the command.
    fn write_status(&mut self, message: &str) -> std::io::Result<()>;

    /// Returns the writer for the command's report output.
    fn report_writer(&mut self) -> &mut (dyn Write + Send);
}

/// Writes command output to the direct command's logging and standard-output sinks.
pub(crate) struct DirectCommandOutput {
    stdout: std::io::Stdout,
}

impl DirectCommandOutput {
    /// Creates output for a command invoked directly from the CLI.
    pub(crate) fn new() -> Self {
        Self {
            stdout: std::io::stdout(),
        }
    }
}

impl CommandOutput for DirectCommandOutput {
    fn write_status(&mut self, message: &str) -> std::io::Result<()> {
        info!("{message}");
        Ok(())
    }

    fn report_writer(&mut self) -> &mut (dyn Write + Send) {
        &mut self.stdout
    }
}

#[cfg(test)]
mod tests {
    use datadog_protos::agent::command::v1::ParameterType;
    use prost_types::{value::Kind, Struct, Value};

    use super::{remote_command_argv, RemoteArgumentType, RemoteCommandDescriptor, RemoteParameterDescriptor};

    const PARAMETERS: &[RemoteParameterDescriptor] = &[
        RemoteParameterDescriptor {
            name: "string",
            short_name: "s",
            helper: "A string.",
            argument_type: RemoteArgumentType::String,
            required: true,
        },
        RemoteParameterDescriptor {
            name: "bool",
            short_name: "b",
            helper: "A boolean.",
            argument_type: RemoteArgumentType::Bool,
            required: false,
        },
        RemoteParameterDescriptor {
            name: "uint",
            short_name: "u",
            helper: "An unsigned integer.",
            argument_type: RemoteArgumentType::Uint,
            required: false,
        },
    ];
    const COMMANDS: &[RemoteCommandDescriptor] = &[RemoteCommandDescriptor {
        name: "inspect",
        helper: "Inspect a service.",
        parameters: PARAMETERS,
    }];

    #[test]
    fn remote_command_metadata_preserves_descriptor_details() {
        let command = COMMANDS[0].to_rcp_command();

        assert_eq!(command.name, "inspect");
        assert_eq!(command.short_name, "inspect");
        assert_eq!(command.helper, "Inspect a service.");
        assert!(command.is_runnable);
        assert_eq!(command.parameters.len(), 3);
        assert_eq!(command.parameters[0].name, "string");
        assert_eq!(command.parameters[0].short_name, "s");
        assert_eq!(command.parameters[0].helper, "A string.");
        assert_eq!(
            ParameterType::try_from(command.parameters[0].r#type).expect("parameter type should be valid"),
            ParameterType::TypeString
        );
        assert!(command.parameters[0].required);
        assert!(command
            .parameters
            .iter()
            .all(|parameter| parameter.is_flag && !parameter.is_persistent));
        assert_eq!(
            ParameterType::try_from(command.parameters[1].r#type).expect("parameter type should be valid"),
            ParameterType::TypeBool
        );
        assert_eq!(
            ParameterType::try_from(command.parameters[2].r#type).expect("parameter type should be valid"),
            ParameterType::TypeUint
        );
    }

    #[test]
    fn remote_command_argv_validates_types_and_uses_descriptor_order() {
        let arguments = Struct {
            fields: [
                ("uint".to_string(), value(Kind::NumberValue(42.0))),
                ("string".to_string(), value(Kind::StringValue("value".to_string()))),
                ("bool".to_string(), value(Kind::BoolValue(true))),
            ]
            .into(),
        };

        let argv = remote_command_argv(&["inspect".to_string()], &arguments, COMMANDS, "test")
            .expect("supported typed arguments should convert to argv");

        assert_eq!(argv, ["inspect", "--string", "value", "--bool", "true", "--uint", "42"]);
    }

    #[test]
    fn remote_command_argv_rejects_unsupported_argument_values() {
        let arguments = Struct {
            fields: [("uint".to_string(), value(Kind::NumberValue(u64::MAX as f64)))].into(),
        };

        let error = remote_command_argv(&["inspect".to_string()], &arguments, COMMANDS, "test")
            .expect_err("the unrepresentable unsigned-integer upper bound must be rejected");

        assert!(error.to_string().contains("unsigned integer"));
    }

    fn value(kind: Kind) -> Value {
        Value { kind: Some(kind) }
    }
}
