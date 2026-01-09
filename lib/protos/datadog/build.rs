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

fn generate_payload_builder_definitions() {
    piecemeal_build::ConfigBuilder::new()
        .input_files(&["proto/agent-payload/agent_payload.proto"])
        .include_paths(&["proto", "proto/agent-payload"])
        .cargo_output_dir("payload_builder")
        .expect("Failed to configure output directory.")
        .compile()
        .expect("Failed to compile .proto files.");
}

fn get_protobuf_codegen_customize_config() -> protobuf_codegen::Customize {
    protobuf_codegen::Customize::default()
        .generate_accessors(true)
        .gen_mod_rs(true)
        .lite_runtime(true)
}

fn generate_payload_protobuf_definitions() {
    protobuf_codegen::Codegen::new()
        .protoc()
        .includes(["proto", "proto/agent-payload"])
        .inputs(["proto/agent-payload/agent_payload.proto"])
        .cargo_out_dir("payload_protos")
        .customize(get_protobuf_codegen_customize_config())
        .run_from_script();
}

fn generate_trace_protobuf_definitions() {
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
        .customize(get_protobuf_codegen_customize_config())
        .customize_callback(SerdeCapableStructs)
        .run_from_script();
}

fn generate_remote_agent_grpc_bindings() {
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

fn main() {
    // Always rerun if the build script itself changes.
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=proto");

    // Handle builder code generate for the payload-specific definitions.
    generate_payload_builder_definitions();

    // Handle code generation for payload and trace-specific definitions.
    //
    // This has to happen separately because the underlying library for generating the bindings can't write to the same
    // output directory without overwriting relevant files.
    generate_payload_protobuf_definitions();
    generate_trace_protobuf_definitions();

    // Handle code generation for gRPC service definitions.
    generate_remote_agent_grpc_bindings();
}
