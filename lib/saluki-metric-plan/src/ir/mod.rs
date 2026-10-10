//! The logical intermediate representation (IR) of plans.
//!
//! A [`Plan`] is a directed acyclic graph of [`Node`]s, each running a registered operator with typed parameters,
//! connected by [`Edge`]s from one node's output port to another node's input port. Paths end in `emit` nodes, which
//! deliver to declared [`OutputDecl`]s, or in `drop` nodes. [`Scopes`] name the regions of a plan that a
//! [`PolicyArtifact`] may replace.
//!
//! The IR is serialized as JSON. Every object rejects unknown members, and [`plan_schema`],
//! [`policy_artifact_schema`], and [`predicate_schema`] generate its JSON Schema, which is checked into the crate's
//! `schema` directory. Documents
//! carry an `ir_version`; [`migrate`] brings documents of older [accepted versions][ACCEPTED_IR_VERSIONS] up to
//! [`IR_VERSION`].
//!
//! Configuration-bindable parameters are [`Operand`]s: a literal, or a binding `{"config": "<binding>"}` that lowering
//! resolves from configuration. Within parameters, the member names `config` and `blob` are reserved, for bindings and
//! for an artifact's [blob](Blobs) references.
//!
//! Documents are read with [`Plan::from_json`] and [`PolicyArtifact::from_json`] (or [`ReceivedArtifact::from_json`],
//! which also keeps the hash that approval binds to). An object that repeats a member name is rejected, so that every
//! reader of a document sees the same document.

mod document;
pub(crate) use self::document::parse_document;

mod id;
pub use self::id::{
    derived_terminal_id, is_identifier, ArtifactNodeId, ArtifactPortRef, BindingName, CredentialRef, ExitName, IdError,
    InputName, LocalId, Namespace, NativeComponentId, NodeId, OperatorName, OutputId, ParamName, PortName, PortRef,
    RecipientId, ScopeName, SignalName, IDENTIFIER_PATTERN, MAX_IDENTIFIER_LEN, SCOPE_EXIT_NODE, SCOPE_INPUT_NODE,
};

mod operand;
pub use self::operand::Operand;

mod params;
pub use self::params::{
    decode_params, format_duration, parse_duration, Decimal, DecodedParams, ParamError, ParamSpec, ParamType,
    ParamValue, Params,
};

mod plan;
pub use self::plan::{
    ArtifactBase, ArtifactEdge, ArtifactNode, ArtifactRevision, BlobError, Blobs, Edge, Node, OutputDecl, Plan,
    PlanProvenance, PolicyArtifact, Provenance, ReceivedArtifact, ScopeDecl, Scopes, BLOB_MEMBER,
};

mod schema;
pub use self::schema::{plan_schema, policy_artifact_schema, predicate_schema};

mod value_type;
pub use self::value_type::{MemberType, ValueType};

mod version;
pub use self::version::{migrate, IrError, MigrationStep, ACCEPTED_IR_VERSIONS, IR_VERSION, MIGRATIONS};
