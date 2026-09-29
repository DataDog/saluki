//! The closed lists of names the corpus may use, each tied to where it is defined.
//!
//! These lists were reviewed against the Agent at [`REVIEWED_AT_AGENT_COMMIT`]. When the pin moves,
//! re-check every list here against the new pin before bumping that constant.

use std::fmt;

/// The Agent commit these lists were last checked against.
pub const REVIEWED_AT_AGENT_COMMIT: &str = "281d921619d52ce7b99aef40607285992c9c2e89";

/// Declares a string enum with its full list and its string forms.
macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident, $all:ident { $($(#[$vmeta:meta])* $variant:ident => $text:literal,)* }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $($(#[$vmeta])* $variant,)*
        }

        impl $name {
            /// Every variant, in declaration order.
            pub const $all: &'static [$name] = &[$($name::$variant,)*];

            /// The name as the corpus writes it.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text,)*
                }
            }

            /// Parses the name as the corpus writes it.
            pub fn parse(s: &str) -> Option<Self> {
                match s {
                    $($text => Some($name::$variant),)*
                    _ => None,
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum! {
    /// A `model.Source` string (`pkg/config/model/types.go:29-59` at the pin).
    Source, ALL {
        /// `schema`.
        Schema => "schema",
        /// `default`.
        Default => "default",
        /// `unknown`.
        Unknown => "unknown",
        /// `infra-mode`.
        InfraMode => "infra-mode",
        /// `file`.
        File => "file",
        /// `environment-variable`.
        EnvironmentVariable => "environment-variable",
        /// `config-post-init`.
        ConfigPostInit => "config-post-init",
        /// `secret`.
        Secret => "secret",
        /// `local-config-process`.
        LocalConfigProcess => "local-config-process",
        /// `agent-runtime`.
        AgentRuntime => "agent-runtime",
        /// `remote-config`.
        RemoteConfig => "remote-config",
        /// `fleet-policies`.
        FleetPolicies => "fleet-policies",
        /// `cli`.
        Cli => "cli",
        /// `provided`.
        Provided => "provided",
        /// The empty string: a streamed setting's `source` is written even when `""` (record.md §5.1).
        /// Only legal there; a read source, an update source or an `unset_source` must never be `""`.
        Empty => "",
    }
}

impl Source {
    /// The sources a case update may write or clear (case.md §5; `pkg/config/model/types.go:37-57`).
    pub const UPDATE: &'static [Source] = &[
        Source::InfraMode,
        Source::File,
        Source::EnvironmentVariable,
        Source::FleetPolicies,
        Source::ConfigPostInit,
        Source::Secret,
        Source::LocalConfigProcess,
        Source::AgentRuntime,
        Source::RemoteConfig,
        Source::Cli,
    ];
}

string_enum! {
    /// An Agent getter a read may call (getter-map.md §2).
    Getter, ALL {
        /// `Get`, returning `interface{}`.
        Get => "Get",
        /// `GetString`, returning `string`.
        GetString => "GetString",
        /// `GetBool`, returning `bool`.
        GetBool => "GetBool",
        /// `GetInt`, returning `int`.
        GetInt => "GetInt",
        /// `GetInt32`, returning `int32`.
        GetInt32 => "GetInt32",
        /// `GetInt64`, returning `int64`.
        GetInt64 => "GetInt64",
        /// `GetFloat64`, returning `float64`.
        GetFloat64 => "GetFloat64",
        /// `GetFloat64Slice`, returning `[]float64`.
        GetFloat64Slice => "GetFloat64Slice",
        /// `GetDuration`, returning `time.Duration`.
        GetDuration => "GetDuration",
        /// `GetStringSlice`, returning `[]string`.
        GetStringSlice => "GetStringSlice",
        /// `GetStringMap`, returning `map[string]interface{}`.
        GetStringMap => "GetStringMap",
        /// `GetStringMapString`, returning `map[string]string`.
        GetStringMapString => "GetStringMapString",
        /// `GetStringMapStringSlice`, returning `map[string][]string`.
        GetStringMapStringSlice => "GetStringMapStringSlice",
        /// `GetSizeInBytes`, returning `uint`.
        GetSizeInBytes => "GetSizeInBytes",
    }
}

string_enum! {
    /// A case's coverage group (case.md §3).
    Group, ALL {
        /// One case with no inputs, recording every modeled key.
        Baseline => "baseline",
        /// Every modeled key, set from each source.
        Breadth => "breadth",
        /// Representative modeled keys, with variants.
        Depth => "depth",
        /// Every unsupported schema key.
        Unsupported => "unsupported",
        /// Representatives of the excluded keys.
        Excluded => "excluded",
        /// Keys that are not in the schema.
        Unknown => "unknown",
        /// Hand-written cases, each naming behavior catalog entries.
        Behavior => "behavior",
    }
}

string_enum! {
    /// A recorded warning's slog level (record.md §7).
    Level, ALL {
        /// `WARN`.
        Warn => "WARN",
        /// `ERROR`.
        Error => "ERROR",
    }
}
