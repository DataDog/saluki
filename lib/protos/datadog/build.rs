use protobuf::descriptor::field_descriptor_proto::Type;
use protobuf::reflect::FieldDescriptor;
use protobuf::reflect::MessageDescriptor;
use protobuf::reflect::RuntimeFieldType;
use protobuf_codegen::Customize;
use protobuf_codegen::CustomizeCallback;

struct SerdeCapableStructs;

impl CustomizeCallback for SerdeCapableStructs {
    fn message(&self, message: &MessageDescriptor) -> Customize {
        println!("Customizing message type '{}'.", message.name());
        Customize::default().before("#[derive(::serde::Serialize, ::serde::Deserialize)]")
    }

    fn field(&self, field: &FieldDescriptor) -> Customize {
        println!(
            "Customizing field '{}': type={:?} type_name={} label={:?}",
            field.name(),
            field.proto().type_(),
            field.proto().type_name(),
            field.proto().label()
        );

        let before_value = match field.proto().type_() {
            // We apply custom (de)serializers for certain `protobuf`-specific types which don't have their own implementation.
            Type::TYPE_ENUM => get_field_serde_annotation(field, Some("enum")),
            Type::TYPE_MESSAGE => match field.runtime_field_type() {
                RuntimeFieldType::Repeated(_) => get_field_serde_annotation(field, Some("repeated")),
                RuntimeFieldType::Map(_, _) => get_field_serde_annotation(field, Some("map")),
                _ => get_field_serde_annotation(field, Some("message")),
            },
            _ => get_field_serde_annotation(field, None),
        };

        Customize::default().before(before_value.as_str())
    }

    fn special_field(&self, _message: &MessageDescriptor, _field: &str) -> Customize {
        Customize::default().before("#[serde(skip)]")
    }
}

fn get_field_serde_annotation(field: &FieldDescriptor, serde_custom_fn_suffix: Option<&str>) -> String {
    // Adjust the case of the field name to match what will be coming from the Go/MessagePack versions of these
    // payloads.
    let field_name = field_name_to_go_case(field.name());

    match serde_custom_fn_suffix {
        Some(suffix) => format!(
            "#[serde(rename = \"{}\", serialize_with = \"crate::serde::serialize_proto_{}\", deserialize_with = \"crate::serde::deserialize_proto_{}\")]",
            field_name, suffix, suffix
        ),
        None => format!("#[serde(rename = \"{}\")]", field_name),
    }
}

fn field_name_to_go_case(field_name: &str) -> String {
    // We split the field name on underscores, capitalize each part, and then smoosh it back together without
    // underscores.
    field_name
        .split('_')
        .map(|part| {
            part.chars()
                .enumerate()
                .map(|(i, c)| if i == 0 { c.to_ascii_uppercase() } else { c })
                .collect::<String>()
        })
        .collect::<Vec<String>>()
        .join("")
}

fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto");

    // Handle code generation for pure Protocol Buffers message types.
    let codegen_customize = protobuf_codegen::Customize::default()
        .generate_accessors(true)
        .gen_mod_rs(true)
        .lite_runtime(true);

    // Two separate invocations are required here because the filename of the trace payload is identical,
    // and `protobuf_codegen` ends up trying to generate code to the same output filename, which
    // means that whichever of the two files that gets processed last overwrites the other.
    protobuf_codegen::Codegen::new()
        .protoc()
        .includes(["proto", "proto/datadog-agent"])
        .inputs(["proto/agent-payload/agent_payload.proto"])
        .cargo_out_dir("protos")
        .customize(codegen_customize.clone())
        .run_from_script();

    protobuf_codegen::Codegen::new()
        .protoc()
        .includes(["proto/datadog-agent"])
        .inputs([
            "proto/datadog-agent/datadog/trace/stats.proto",
            "proto/datadog-agent/datadog/trace/span.proto",
            "proto/datadog-agent/datadog/trace/tracer_payload.proto",
            "proto/datadog-agent/datadog/trace/agent_payload.proto",
        ])
        .cargo_out_dir("trace_protos")
        .customize(codegen_customize)
        .customize_callback(SerdeCapableStructs)
        .run_from_script();

    // Handle code generation for gRPC service definitions.
    tonic_prost_build::configure()
        .build_server(true)
        .include_file("api.mod.rs")
        .compile_protos(
            &[
                "proto/datadog-agent/datadog/api/v1/api.proto",
                "proto/datadog-agent/datadog/remoteagent/status.proto",
                "proto/datadog-agent/datadog/remoteagent/telemetry.proto",
                "proto/datadog-agent/datadog/remoteagent/flare.proto",
            ],
            &["proto", "proto/datadog-agent"],
        )
        .expect("Failed to build gRPC service definitions for Datadog Agent.");
}
