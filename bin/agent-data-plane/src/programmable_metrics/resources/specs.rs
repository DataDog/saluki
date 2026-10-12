//! The typed specifications the encode and delivery hosts run, read from what the installer core decides.
//!
//! The core names leases, recipients, and host settings as plan declarations and parameters; the hosts run the typed
//! specifications of `saluki-components`. This module translates one into the other, through the
//! [codec](super::params):
//!
//! - [`lease_spec`]: a lease to prepare, with the recipients its output resolved to, per protocol variant;
//! - [`recipient_spec`]: a recipient to prepare, reopen, or publish settings for;
//! - [`publication`]: what a delivery host publishes in a resource transaction;
//! - [`delivery_ref`] and [`core_ref`]: a recipient's address, between the core's and a host's types.
//!
//! Every translation is pure, so the installer builds what it sends, and fails a preparation, before anything is sent.

use std::{collections::BTreeSet, num::NonZeroU8, sync::Arc};

use saluki_components::forwarders::{
    delivery::{
        self, DeliverySettings, EncodeLeaseSpec, LeaseKeyDigest, PayloadClasses as Classes, PayloadRecipients,
        RecipientOrigin, RecipientPolicy, RecipientSettings, RecipientSlot as Slot, RecipientSpec, ResolvedRecipients,
        RetireClosure, SpillPolicy,
    },
    MetricsRecipient, MetricsRecipientRoute,
};
use saluki_error::{generic_error, ErrorContext as _, GenericError};
use saluki_metric_plan::{
    extension::{Closure, DeliveryPolicy, Spill},
    install::{HostPublish, LeasePrepare, RecipientPrepare, RecipientRef, RecipientSlot, RetirementReport},
    ir::{HostBinding, PayloadClasses, RecipientDecl, RecipientId, Resources},
    resources::CREDENTIALS_NAMESPACE,
};
use stringtheory::MetaString;

use super::params::{decode_encoder, decode_host_settings, decode_protocol};

/// Returns a recipient's address as a delivery host takes it.
pub fn delivery_ref(recipient: &RecipientRef) -> delivery::RecipientRef {
    delivery::RecipientRef {
        slot: Slot::new(recipient.slot.get()),
        id: recipient.id.as_str().into(),
        incarnation: recipient.incarnation,
    }
}

/// Returns a recipient's address, as a delivery host reports it, as the installer core takes it.
///
/// # Errors
///
/// Returns an error if the identity is not a recipient identity, which no host the installer drives reports.
pub fn core_ref(recipient: &delivery::RecipientRef) -> Result<RecipientRef, GenericError> {
    let id = RecipientId::new(recipient.id.to_string())
        .map_err(|error| generic_error!("A delivery host reported recipient `{}`: {}", recipient.id, error))?;
    Ok(RecipientRef {
        slot: RecipientSlot::new(recipient.slot.get()),
        id,
        incarnation: recipient.incarnation,
    })
}

/// Returns what a retired recipient's drain did, as the installer core takes it.
pub fn retirement_report(report: &delivery::DrainReport) -> RetirementReport {
    RetirementReport {
        delivered: report.delivered,
        persisted: report.persisted,
        dropped: report.dropped,
        deadline_reached: report.deadline_reached,
    }
}

/// Returns a delivery host's settings, decoded from its binding.
///
/// # Errors
///
/// Returns an error naming the host if its parameters do not decode.
pub fn host_settings(binding: &HostBinding) -> Result<DeliverySettings, GenericError> {
    decode_host_settings(&binding.settings).error_context(format!(
        "Failed to read the settings of delivery host `{}`.",
        binding.host
    ))
}

/// Returns a recipient's delivery policy as its host applies it.
pub fn recipient_policy(policy: &DeliveryPolicy) -> RecipientPolicy {
    match policy {
        DeliveryPolicy::Native => RecipientPolicy::default(),
        DeliveryPolicy::Bounded(bounded) => RecipientPolicy {
            queue_bytes: bounded.queue_bytes,
            spill: match bounded.spill {
                Spill::Native => SpillPolicy::Native,
                Spill::Never => SpillPolicy::Never,
            },
            on_retire: match bounded.on_retire {
                Closure::Native => RetireClosure::Native,
                Closure::Attempt => RetireClosure::Attempt,
                Closure::Persist => RetireClosure::Persist,
                Closure::Discard => RetireClosure::Discard,
            },
        },
    }
}

/// Returns the specification of a recipient a delivery host prepares, reopens, or publishes settings for.
///
/// A native recipient's origin is the forwarder's own recipient, which its identity spells (`<host>/<route>/<key
/// index>/<endpoint>`) with its configured endpoint: its task and its queue directory are named after it, as natively.
/// A declared recipient's is its name in the resources artifact, `declared_name`, or, when that is not known, its
/// credential's name, and its credential's name.
///
/// # Errors
///
/// Returns an error naming the recipient if its protocol does not decode, or if its identity or credential is not of
/// its provenance's form.
pub fn recipient_spec(decl: &RecipientDecl, declared_name: Option<&str>) -> Result<RecipientSpec, GenericError> {
    let invalid = |what: &str| generic_error!("Recipient `{}` cannot be prepared: {}.", decl.id, what);
    let protocol = decode_protocol(&decl.protocol)
        .error_context(format!("Failed to read the protocol of recipient `{}`.", decl.id))?;
    let classes = match decl.classes {
        PayloadClasses::MetricsOnly => Classes::MetricsOnly,
        PayloadClasses::All => Classes::All,
    };
    let origin = match decl.credential.as_str().strip_prefix(CREDENTIALS_NAMESPACE) {
        Some(credential) => RecipientOrigin::Declared {
            name: declared_name.unwrap_or(credential).into(),
            credential: credential.into(),
        },
        None => {
            let mut segments = decl.id.as_str().splitn(4, '/');
            let (_host, route, index) = (segments.next(), segments.next(), segments.next());
            let route = match route {
                Some("primary") => MetricsRecipientRoute::Primary,
                Some("metrics_override") => MetricsRecipientRoute::MetricsOverride,
                Some("additional") => MetricsRecipientRoute::Additional,
                _ => return Err(invalid("its identity names no native route")),
            };
            let index = index
                .and_then(|index| index.parse::<usize>().ok())
                .ok_or_else(|| invalid("its identity names no key index"))?;
            RecipientOrigin::Native(MetricsRecipient {
                route,
                configured_endpoint: decl.endpoint.clone(),
                api_key_index: (route == MetricsRecipientRoute::Additional).then_some(index),
                classes,
                protocol: protocol.clone(),
            })
        }
    };
    Ok(RecipientSpec {
        id: decl.id.as_str().into(),
        endpoint: decl.endpoint.clone(),
        credential: decl.credential.as_str().into(),
        classes,
        protocol,
        policy: recipient_policy(&decl.delivery),
        origin,
    })
}

/// Returns the specification of a lease an encode host prepares: its key, its encoder settings, the identity its
/// telemetry reports under, and, per protocol variant, the slots of the recipients its output resolved to that take
/// the variant, in the order the core lists them.
///
/// `recipients` holds the declaration of every recipient the lease names: the plan's resources.
///
/// # Errors
///
/// Returns an error naming the output if it has no binding, if its encoder parameters do not decode, or if it names a
/// recipient `recipients` does not declare.
pub fn lease_spec(prepare: &LeasePrepare, recipients: &Resources) -> Result<EncodeLeaseSpec, GenericError> {
    let output = &prepare.output;
    let binding = output.binding.as_ref().ok_or_else(|| {
        generic_error!(
            "Output `{}` cannot run as a lease: it is bound to no delivery host.",
            output.id
        )
    })?;
    let settings = decode_encoder(&binding.encoder).error_context(format!(
        "Failed to read the encoder settings of output `{}`.",
        output.id
    ))?;
    let mut lists: [Vec<Slot>; 4] = Default::default();
    for recipient in &prepare.recipients {
        let decl = recipients.recipient(&recipient.id).ok_or_else(|| {
            generic_error!(
                "Output `{}` names recipient `{}`, which the plan does not declare.",
                output.id,
                recipient.id
            )
        })?;
        let v3 = decode_protocol(&decl.protocol)
            .error_context(format!("Failed to read the protocol of recipient `{}`.", decl.id))?
            .v3();
        let slot = Slot::new(recipient.slot.get());
        let [v2_series, v2_sketches, v3_series, v3_sketches] = &mut lists;
        if v3.series { v3_series } else { v2_series }.push(slot);
        if v3.sketches { v3_sketches } else { v2_sketches }.push(slot);
    }
    let [v2_series, v2_sketches, v3_series, v3_sketches] = lists.map(|slots| PayloadRecipients::new(slots.into()));
    Ok(EncodeLeaseSpec {
        lease: prepare.lease.lease,
        key: LeaseKeyDigest {
            output: prepare.lease.key.output.as_str().into(),
            binding: *prepare.lease.key.binding.digest(),
        },
        telemetry_id: binding.telemetry.as_str().into(),
        settings,
        recipients: Arc::new(ResolvedRecipients {
            v2_series,
            v2_sketches,
            v3_series,
            v3_sketches,
        }),
        builders: NonZeroU8::MIN,
    })
}

/// Returns one recipient's settings, as a reopen or a publication applies them.
///
/// # Errors
///
/// Returns an error naming the recipient if its specification cannot be read (see [`recipient_spec`]).
pub fn recipient_settings(
    prepare: &RecipientPrepare, declared_name: Option<&str>,
) -> Result<RecipientSettings, GenericError> {
    Ok(RecipientSettings {
        recipient: delivery_ref(&prepare.recipient),
        spec: recipient_spec(&prepare.decl, declared_name)?,
    })
}

/// What a delivery host publishes, as its control message carries it.
#[derive(Clone, Debug, PartialEq)]
pub struct Publication {
    /// The revision published.
    pub revision: u64,

    /// The native set.
    pub native_set: Vec<delivery::RecipientRef>,

    /// The host's settings.
    pub settings: Arc<DeliverySettings>,

    /// Every recipient's settings.
    pub recipients: Vec<RecipientSettings>,

    /// The withdrawn recipients, by identity.
    pub withdrawn: BTreeSet<MetaString>,
}

/// Returns what a delivery host publishes for revision `revision` of a resource transaction, or at boot.
///
/// `declared_name` returns the name a resources artifact declares a recipient under, if it is known.
///
/// # Errors
///
/// Returns an error naming the host or recipient whose parameters do not decode.
pub fn publication(
    revision: u64, publish: &HostPublish, declared_name: impl Fn(&RecipientId) -> Option<String>,
) -> Result<Publication, GenericError> {
    let recipients = publish
        .recipients
        .iter()
        .map(|prepare| recipient_settings(prepare, declared_name(&prepare.decl.id).as_deref()))
        .collect::<Result<_, _>>()?;
    Ok(Publication {
        revision,
        native_set: publish.native_set.iter().map(delivery_ref).collect(),
        settings: Arc::new(host_settings(&publish.settings)?),
        recipients,
        withdrawn: publish.withdrawn.iter().map(|id| id.as_str().into()).collect(),
    })
}

#[cfg(test)]
mod tests {
    use saluki_components::forwarders::delivery::{IntakeAuthorities, RecipientProtocol, V3Acceptance};
    use saluki_core::execution::EntryId;
    use saluki_metric_plan::{
        canonical::PlanHash,
        extension::BoundedDelivery,
        install::{LeaseKey, LeaseRef},
        ir::{CredentialRef, HostId, OutputBinding, OutputDecl, OutputId, ResourceProvenance, RoleName, Selector},
        model::PortType,
    };

    use super::{
        super::params::{encode_encoder, encode_protocol},
        *,
    };

    const HASH: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    fn intake(series: bool, sketches: bool) -> RecipientProtocol {
        RecipientProtocol::DatadogIntake {
            authorities: IntakeAuthorities::default(),
            allow_arbitrary_tags: false,
            v3: V3Acceptance { series, sketches },
        }
    }

    fn decl(id: &str, endpoint: &str, credential: &str, protocol: &RecipientProtocol) -> RecipientDecl {
        RecipientDecl {
            id: RecipientId::new(id).unwrap(),
            host: HostId::new("dd_out").unwrap(),
            endpoint: endpoint.to_string(),
            credential: CredentialRef::new(credential).unwrap(),
            classes: PayloadClasses::All,
            protocol: encode_protocol(protocol),
            delivery: DeliveryPolicy::Native,
            provenance: ResourceProvenance::Native,
        }
    }

    fn recipient(slot: u16, id: &str) -> RecipientRef {
        RecipientRef {
            slot: RecipientSlot::new(slot),
            id: RecipientId::new(id).unwrap(),
            incarnation: u64::from(slot) + 10,
        }
    }

    #[test]
    fn native_recipients_name_their_route_key_index_and_configured_endpoint() {
        let additional = decl(
            "dd_out/additional/1/https://dual.example.com",
            "https://dual.example.com",
            "native:additional/fa8d2eae247dcfe1/1",
            &intake(true, false),
        );
        let spec = recipient_spec(&additional, None).unwrap();
        assert_eq!(&*spec.id, "dd_out/additional/1/https://dual.example.com");
        assert_eq!(&*spec.credential, "native:additional/fa8d2eae247dcfe1/1");
        assert_eq!(spec.classes, Classes::All);
        assert_eq!(spec.protocol, intake(true, false));
        assert_eq!(spec.policy, RecipientPolicy::default());
        assert_eq!(
            spec.origin,
            RecipientOrigin::Native(MetricsRecipient {
                route: MetricsRecipientRoute::Additional,
                configured_endpoint: "https://dual.example.com".to_string(),
                api_key_index: Some(1),
                classes: Classes::All,
                protocol: intake(true, false),
            })
        );

        // The primary has one key, and no index; a long endpoint's digest in the identity leaves the configured
        // endpoint as it is.
        let long = format!("https://{}.example.com", "a".repeat(400));
        let primary = decl(
            "dd_out/primary/0/sha256:0123456789abcdef",
            &long,
            "native:api_key",
            &intake(false, false),
        );
        let RecipientOrigin::Native(native) = recipient_spec(&primary, None).unwrap().origin else {
            panic!("a native recipient");
        };
        assert_eq!(native.route, MetricsRecipientRoute::Primary);
        assert_eq!(native.api_key_index, None);
        assert_eq!(native.configured_endpoint, long);

        let unroutable = decl("dd_out/elsewhere/0/x", "x", "native:api_key", &intake(false, false));
        assert_eq!(
            recipient_spec(&unroutable, None).unwrap_err().to_string(),
            "Recipient `dd_out/elsewhere/0/x` cannot be prepared: its identity names no native route."
        );
    }

    #[test]
    fn declared_recipients_carry_their_names_and_their_bounded_policy() {
        let mut team_d = decl(
            "dd_out/declared/team_d/http://127.0.0.1:17105",
            "http://127.0.0.1:17105",
            "credentials/team_d",
            &intake(false, false),
        );
        team_d.classes = PayloadClasses::MetricsOnly;
        team_d.delivery = DeliveryPolicy::Bounded(BoundedDelivery {
            queue_bytes: Some(4_194_304),
            spill: Spill::Never,
            on_retire: Closure::Discard,
        });
        let spec = recipient_spec(&team_d, Some("team_d_intake")).unwrap();
        assert_eq!(
            spec.origin,
            RecipientOrigin::Declared {
                name: "team_d_intake".into(),
                credential: "team_d".into(),
            }
        );
        assert_eq!(spec.classes, Classes::MetricsOnly);
        assert_eq!(
            spec.policy,
            RecipientPolicy {
                queue_bytes: Some(4_194_304),
                spill: SpillPolicy::Never,
                on_retire: RetireClosure::Discard,
            }
        );
        // Without the artifact's name, the credential's names the recipient's task.
        let RecipientOrigin::Declared { name, .. } = recipient_spec(&team_d, None).unwrap().origin else {
            panic!("a declared recipient");
        };
        assert_eq!(&*name, "team_d");
    }

    #[test]
    fn a_lease_resolves_each_variant_to_the_recipients_that_take_it() {
        let v2 = "dd_out/primary/0/https://app.datadoghq.com";
        let v3 = "dd_out/additional/0/https://dual.example.com";
        let resources = Resources {
            hosts: Vec::new(),
            recipients: vec![
                decl(
                    v3,
                    "https://dual.example.com",
                    "native:additional/x/0",
                    &intake(true, false),
                ),
                decl(v2, "https://app.datadoghq.com", "native:api_key", &intake(false, false)),
            ],
        };
        let settings = saluki_components::forwarders::delivery::EncoderSettings {
            variants: saluki_components::forwarders::delivery::ProtocolVariants {
                v2_series: true,
                v2_sketches: true,
                v3_series: true,
                v3_sketches: false,
            },
            series: saluki_components::forwarders::delivery::SeriesVariant::V2,
            compressor_kind: "zstd".into(),
            zstd_compressor_level: 3,
            v3_compression_level: 0,
            max_metrics_per_payload: 10_000,
            max_payload_size: 2_621_440,
            max_uncompressed_payload_size: 4_194_304,
            max_series_payload_size: 512_000,
            max_series_uncompressed_payload_size: 5_242_880,
            max_series_points_per_payload: 10_000,
            flush_timeout: std::time::Duration::from_secs(2),
            log_payloads: false,
        };
        let output = OutputDecl {
            id: OutputId::new("dd_metrics_encode").unwrap(),
            accepts: PortType::ANY,
            fingerprint: HASH.parse().unwrap(),
            recipients: vec![RecipientId::new(v3).unwrap(), RecipientId::new(v2).unwrap()],
            delivery: DeliveryPolicy::Native,
            binding: Some(OutputBinding {
                host: HostId::new("dd_out").unwrap(),
                selector: Selector::Role(RoleName::new("main").unwrap()),
                encoder: encode_encoder(&settings).unwrap(),
                telemetry: "dd_metrics_encode".parse().unwrap(),
                provenance: ResourceProvenance::Native,
            }),
        };
        let binding: PlanHash = HASH.parse().unwrap();
        let prepare = LeasePrepare {
            lease: LeaseRef {
                lease: EntryId::new(5),
                key: LeaseKey {
                    output: output.id.clone(),
                    binding,
                },
            },
            output: output.clone(),
            recipients: vec![recipient(0, v2), recipient(3, v3)],
        };
        let spec = lease_spec(&prepare, &resources).unwrap();
        assert_eq!(spec.lease, EntryId::new(5));
        assert_eq!(&*spec.key.output, "dd_metrics_encode");
        assert_eq!(&spec.key.binding, binding.digest());
        assert_eq!(&*spec.telemetry_id, "dd_metrics_encode");
        assert_eq!(spec.settings, settings);
        assert_eq!(spec.builders.get(), 1);
        let slots = |list: &PayloadRecipients| list.slots().iter().map(|slot| slot.get()).collect::<Vec<_>>();
        assert_eq!(slots(&spec.recipients.v2_series), [0]);
        assert_eq!(slots(&spec.recipients.v3_series), [3]);
        assert_eq!(slots(&spec.recipients.v2_sketches), [0, 3]);
        assert!(spec.recipients.v3_sketches.slots().is_empty());

        // A recipient the plan does not declare fails the lease.
        let mut stray = prepare.clone();
        stray
            .recipients
            .push(recipient(7, "dd_out/additional/0/https://gone.example.com"));
        assert_eq!(
            lease_spec(&stray, &resources).unwrap_err().to_string(),
            "Output `dd_metrics_encode` names recipient `dd_out/additional/0/https://gone.example.com`, which the \
             plan does not declare."
        );
        let mut unbound = prepare;
        unbound.output.binding = None;
        assert!(lease_spec(&unbound, &resources).is_err());
    }

    #[test]
    fn addresses_and_drain_reports_translate_both_ways() {
        let core = recipient(4, "dd_out/declared/team_d/http://127.0.0.1:17105");
        let host = delivery_ref(&core);
        assert_eq!(host.slot.get(), 4);
        assert_eq!(&*host.id, "dd_out/declared/team_d/http://127.0.0.1:17105");
        assert_eq!(host.incarnation, 14);
        assert_eq!(core_ref(&host).unwrap(), core);
        let invalid = delivery::RecipientRef {
            slot: Slot::new(0),
            id: "has space".into(),
            incarnation: 1,
        };
        assert!(core_ref(&invalid).is_err());

        let report = delivery::DrainReport {
            delivered: 3,
            persisted: 2,
            dropped: 1,
            deadline_reached: true,
        };
        assert_eq!(
            retirement_report(&report),
            RetirementReport {
                delivered: 3,
                persisted: 2,
                dropped: 1,
                deadline_reached: true,
            }
        );
    }
}
