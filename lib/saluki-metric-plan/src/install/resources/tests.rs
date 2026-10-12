//! Resource table fixtures, with expected results written by hand.

use serde_json::{json, Value};

use super::*;
use crate::ir::Plan;

const MAIN: &str = "dd_metrics_encode";
const GROUP: &str = "metrics_routing_encode_0";
const HASH: &str = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

fn dd_out() -> HostId {
    HostId::new("dd_out").unwrap()
}

fn mrf() -> HostId {
    HostId::new("mrf_dd_out").unwrap()
}

/// The identity of the recipient `name` of `host`.
fn id_on(host: &str, name: &str) -> RecipientId {
    RecipientId::new(format!("{}/additional/0/https://{}.example.com", host, name)).unwrap()
}

fn id(name: &str) -> RecipientId {
    id_on("dd_out", name)
}

/// A recipient declaration of `host`: native, or declared by resources revision 3, with V3 series or not.
fn recipient_on(host: &str, name: &str, declared: bool, v3: bool) -> Value {
    let provenance = if declared {
        json!({"declared": {"revision": 3, "hash": HASH}})
    } else {
        json!("native")
    };
    json!({
        "id": id_on(host, name), "host": host, "endpoint": format!("https://{}.example.com", name),
        "credential": "native:api_key", "classes": if declared { "metrics_only" } else { "all" },
        "protocol": {"v3_series": v3}, "provenance": provenance,
    })
}

fn native(name: &str) -> Value {
    recipient_on("dd_out", name, false, false)
}

fn declared(name: &str) -> Value {
    recipient_on("dd_out", name, true, false)
}

/// An output of `host` bound to the recipients `names`, declared by `provenance`.
fn output_on(output: &str, host: &str, names: &[&str], provenance: Value) -> Value {
    let recipients = names.iter().map(|name| id_on(host, name)).collect::<Vec<_>>();
    json!({
        "id": output, "accepts": {"kinds": "all", "timestamps": "mixed"},
        "fingerprint": format!("sha256:{}", "0".repeat(64)), "recipients": recipients,
        "binding": {"host": host, "selector": {"role": "main"}, "telemetry": output, "provenance": provenance},
    })
}

fn output(output: &str, names: &[&str]) -> Value {
    output_on(output, "dd_out", names, json!("native"))
}

fn plan(outputs: Vec<Value>, recipients: Vec<Value>) -> Plan {
    Plan::from_value(json!({
        "ir_version": 1, "nodes": [], "edges": [], "outputs": outputs, "resources": {"recipients": recipients}
    }))
    .unwrap()
}

fn generation(id: u64) -> GenerationId {
    GenerationId::new(id)
}

fn reference(name: &str, slot: u16, incarnation: u64) -> RecipientRef {
    RecipientRef {
        slot: RecipientSlot::new(slot),
        id: id(name),
        incarnation,
    }
}

/// Records and commits a plan for a generation, as a preparation every host acknowledged does.
fn install(table: &mut ResourceTable, generation: GenerationId, candidate: &Plan) -> ResourcePlan {
    let planned = table.plan(candidate);
    table.record(generation, candidate, &planned, BTreeMap::new());
    table.commit(generation);
    planned
}

/// Runs a transaction for a lowering whose every host prepares its part, returning what it published.
fn transact(table: &mut ResourceTable, lowered: &Plan) -> Completed {
    let transaction = table
        .transaction(&lowered.resources)
        .expect("the lowering needs a transaction");
    assert!(table.begin(&transaction, &lowered.resources).is_none());
    for part in &transaction.prepare {
        table.answer(transaction.revision, &part.host, PartAnswer::Prepared);
    }
    table.complete().expect("a transaction is in flight")
}

/// The state of every recipient of the table, by name, slot, and incarnation.
fn states(table: &ResourceTable) -> Vec<(String, u16, u64, RecipientState)> {
    table
        .recipients()
        .into_iter()
        .map(|status| {
            let name = status
                .recipient
                .id
                .as_str()
                .rsplit("https://")
                .next()
                .unwrap()
                .trim_end_matches(".example.com")
                .to_string();
            (
                name,
                status.recipient.slot.get(),
                status.recipient.incarnation,
                status.state,
            )
        })
        .collect()
}

fn lease_of(planned: &ResourcePlan, output: &str) -> EntryId {
    planned.leases.lease(&OutputId::new(output).unwrap()).unwrap()
}

#[derive(Debug)]
struct Bounds;

impl ResourceBounds for Bounds {
    fn lease(&self, _: &HostId, _: &OutputDecl) -> u64 {
        100
    }

    fn recipient(&self, recipient: &RecipientDecl, _: Option<&HostBinding>) -> u64 {
        match recipient.provenance {
            ResourceProvenance::Declared(_) => 10,
            ResourceProvenance::Native => 0,
        }
    }

    fn host_queue(&self, _: &HostId, _: Option<&HostBinding>) -> u64 {
        1_000
    }
}

#[test]
fn a_generation_reuses_a_live_lease_of_an_equal_binding_and_a_changed_binding_gets_another() {
    let mut table = ResourceTable::new();
    let first = install(
        &mut table,
        generation(1),
        &plan(vec![output(MAIN, &["a"])], vec![native("a")]),
    );
    assert_eq!(
        first.hosts,
        vec![HostPrepare {
            host: dd_out(),
            leases: vec![LeasePrepare {
                lease: LeaseRef {
                    lease: EntryId::new(0),
                    key: LeaseKey::of(
                        &serde_json::from_value(output(MAIN, &["a"])).unwrap(),
                        &plan(vec![], vec![native("a")]).resources
                    )
                    .unwrap(),
                },
                output: serde_json::from_value(output(MAIN, &["a"])).unwrap(),
                recipients: vec![reference("a", 0, 1)],
            }],
            create: vec![RecipientPrepare {
                recipient: reference("a", 0, 1),
                decl: serde_json::from_value(native("a")).unwrap(),
            }],
            reopen: vec![],
            requires: vec![],
        }]
    );

    // The same binding, declared by a resources revision: provenance is not part of the key, so the lease and its
    // recipient are reused, and the recipient is required live.
    let again = plan(
        vec![output_on(
            MAIN,
            "dd_out",
            &["a"],
            json!({"declared": {"revision": 3, "hash": HASH}}),
        )],
        vec![native("a")],
    );
    let second = install(&mut table, generation(2), &again);
    assert_eq!(lease_of(&second, MAIN), EntryId::new(0));
    assert_eq!(second.hosts.len(), 1);
    assert!(second.hosts[0].leases.is_empty() && second.hosts[0].create.is_empty());
    assert_eq!(second.hosts[0].requires, vec![reference("a", 0, 1)]);

    // A recipient added to the output changes its binding: another lease, under another identifier.
    let third = install(
        &mut table,
        generation(3),
        &plan(vec![output(MAIN, &["a", "b"])], vec![native("a"), native("b")]),
    );
    assert_eq!(lease_of(&third, MAIN), EntryId::new(1));
    assert_eq!(third.hosts[0].create[0].recipient, reference("b", 1, 2));

    // Once the generations using lease 0 retire, it drains; the binding declared again gets a new lease beside it.
    assert!(
        table.retire(generation(1)).leases.is_empty(),
        "generation 2 still uses lease 0"
    );
    let released = table.retire(generation(2));
    assert_eq!(released.leases[&dd_out()][0].lease, EntryId::new(0));
    let fourth = table.plan(&plan(vec![output(MAIN, &["a"])], vec![native("a")]));
    assert_eq!(
        lease_of(&fourth, MAIN),
        EntryId::new(2),
        "a draining lease is never reused"
    );
}

#[test]
fn retirement_drains_leases_first_and_releases_a_recipient_once_its_last_lease_is_released() {
    let mut table = ResourceTable::new();
    install(
        &mut table,
        generation(1),
        &plan(vec![output(MAIN, &["a", "b"])], vec![native("a"), native("b")]),
    );
    install(
        &mut table,
        generation(2),
        &plan(vec![output(MAIN, &["a"])], vec![native("a")]),
    );

    // Generation 1 retires: its lease drains, and `b`, which only that lease names, waits for the drain.
    let released = table.retire(generation(1));
    let lease = released.leases[&dd_out()][0].clone();
    assert!(released.recipients.is_empty());
    assert_eq!(states(&table)[1], ("b".to_string(), 1, 2, RecipientState::Live));

    // A release report with another key changes nothing; the lease's own release frees `b`.
    let mut other = lease.clone();
    other.key.binding = PlanHash::of_bytes(b"other");
    assert_eq!(table.lease_released(&dd_out(), &other), None);
    let released = table.lease_released(&dd_out(), &lease).unwrap();
    assert_eq!(released.recipients[&dd_out()], vec![reference("b", 1, 2)]);
    assert_eq!(
        table.lease_released(&dd_out(), &lease),
        None,
        "a lease is released once"
    );
    assert_eq!(states(&table)[1].3, RecipientState::Draining);

    // Its exit frees the slot; an exit reported again, or of an older incarnation, changes nothing.
    assert!(table.recipient_retired(&reference("b", 1, 2)).is_some());
    assert!(table.recipient_retired(&reference("b", 1, 2)).is_none());
    assert!(table.recipient_retired(&reference("a", 0, 0)).is_none());
    assert_eq!(states(&table), vec![("a".to_string(), 0, 1, RecipientState::Live)]);
}

#[test]
fn an_abort_drops_what_only_its_generation_created_and_returns_a_reopened_recipient_to_its_drain() {
    let mut table = ResourceTable::new();
    install(
        &mut table,
        generation(1),
        &plan(vec![output(MAIN, &["a"])], vec![native("a")]),
    );
    install(
        &mut table,
        generation(2),
        &plan(vec![output(MAIN, &["b"])], vec![native("b")]),
    );
    let lease = table.retire(generation(1)).leases[&dd_out()][0].clone();
    table.lease_released(&dd_out(), &lease).unwrap();
    assert_eq!(states(&table)[0].3, RecipientState::Draining);

    // Generation 3 reopens `a`, requires `b`, and creates `c`; it is aborted.
    let candidate = plan(
        vec![output(MAIN, &["a", "b", "c"])],
        vec![native("a"), native("b"), native("c")],
    );
    let planned = table.plan(&candidate);
    let part = &planned.hosts[0];
    assert_eq!(part.reopen[0].recipient, reference("a", 0, 1));
    assert_eq!(part.requires, vec![reference("b", 1, 2)]);
    assert_eq!(part.create[0].recipient, reference("c", 2, 3));
    table.record(generation(3), &candidate, &planned, BTreeMap::new());
    assert_eq!(
        states(&table)[0].3,
        RecipientState::Preparing,
        "reopened, not committed"
    );

    let released = table.abort(generation(3));
    assert_eq!(released.leases[&dd_out()].len(), 1, "its new lease drains");
    assert_eq!(
        states(&table),
        vec![
            ("a".to_string(), 0, 1, RecipientState::Draining),
            ("b".to_string(), 1, 2, RecipientState::Live),
        ]
    );
    assert!(table.hosts_of(generation(3)).is_empty());
    // The exit the host reports for `c`, which it dropped, is ignored.
    assert!(table.recipient_retired(&reference("c", 2, 3)).is_none());
}

#[test]
fn a_recipient_two_preparations_create_stays_until_both_abort_and_is_committed_by_either() {
    let mut table = ResourceTable::new();
    let lowered = plan(vec![], vec![native("a")]);
    let transaction = table.transaction(&lowered.resources).unwrap();
    assert_eq!(transaction.prepare[0].create[0].recipient, reference("a", 0, 1));
    table.begin(&transaction, &lowered.resources);

    // A generation planned while the transaction prepares `a` creates it again under the same reference.
    let candidate = plan(vec![output(MAIN, &["a"])], vec![native("a")]);
    let planned = table.plan(&candidate);
    assert_eq!(planned.hosts[0].create[0].recipient, reference("a", 0, 1));
    assert!(planned.hosts[0].requires.is_empty());
    table.record(generation(2), &candidate, &planned, BTreeMap::new());

    // The generation's abort keeps it for the transaction, and the transaction's publication commits it.
    table.abort(generation(2));
    assert_eq!(states(&table), vec![("a".to_string(), 0, 1, RecipientState::Preparing)]);
    table.answer(transaction.revision, &dd_out(), PartAnswer::Prepared);
    let completed = table.complete().unwrap();
    assert_eq!(completed.publish[0].native_set, vec![reference("a", 0, 1)]);
    assert_eq!(states(&table), vec![("a".to_string(), 0, 1, RecipientState::Live)]);

    // The other way around: a generation that commits first commits it, and the transaction's abort keeps it.
    let mut table = ResourceTable::new();
    let lowered = plan(vec![], vec![native("a")]);
    let transaction = table.transaction(&lowered.resources).unwrap();
    table.begin(&transaction, &lowered.resources);
    install(&mut table, generation(2), &candidate);
    let (_, hosts, released) = table.supersede().unwrap();
    assert_eq!(hosts, vec![dd_out()]);
    assert!(released.credits.is_empty() && released.recipients.is_empty());
    assert_eq!(states(&table), vec![("a".to_string(), 0, 1, RecipientState::Live)]);
}

#[test]
fn a_transaction_prepares_additions_publishes_removals_and_withdraws_what_the_lowering_no_longer_declares() {
    let mut table = ResourceTable::new();
    let both = plan(vec![], vec![native("a"), native("b")]);
    let transaction = table.transaction(&both.resources).unwrap();
    assert_eq!(transaction.revision, ResourcesRevision::new(1));
    assert_eq!(
        transaction.prepare[0]
            .create
            .iter()
            .map(|create| create.recipient.clone())
            .collect::<Vec<_>>(),
        vec![reference("a", 0, 1), reference("b", 1, 2)]
    );
    let completed = transact(&mut table, &both);
    assert_eq!(
        completed.publish[0].native_set,
        vec![reference("a", 0, 1), reference("b", 1, 2)]
    );
    assert!(completed.publish[0].withdrawn.is_empty());
    assert_eq!(
        table.published(&dd_out()).map(|(revision, _)| revision),
        Some(ResourcesRevision::new(1))
    );
    assert_eq!(
        table.transaction(&both.resources),
        None,
        "nothing differs from what was published"
    );

    // `b` removed: nothing to prepare, so the publication follows at once, withdraws `b`, and releases it, since
    // nothing else holds it.
    let one = plan(vec![], vec![native("a")]);
    let transaction = table.transaction(&one.resources).unwrap();
    assert!(transaction.prepare.is_empty());
    let completed = transact(&mut table, &one);
    assert_eq!(completed.publish[0].native_set, vec![reference("a", 0, 1)]);
    assert_eq!(completed.publish[0].withdrawn, BTreeSet::from([id("b")]));
    assert_eq!(completed.released.recipients[&dd_out()], vec![reference("b", 1, 2)]);

    // Once `b` retired, a publication drops its stale withdrawal, which would otherwise cover the next incarnation a
    // generation prepares for it; declaring it again restores it.
    table.recipient_retired(&reference("b", 1, 2)).unwrap();
    let completed = transact(&mut table, &one);
    assert!(completed.publish[0].withdrawn.is_empty());
    assert_eq!(table.transaction(&one.resources), None);
    let completed = transact(&mut table, &both);
    assert_eq!(
        completed.publish[0].native_set,
        vec![reference("a", 0, 1), reference("b", 2, 3)],
        "slots are allocated in rotation"
    );
    assert!(completed.publish[0].withdrawn.is_empty());
}

#[test]
fn an_addition_that_fails_is_left_out_while_the_removals_and_withdrawals_publish() {
    let mut table = ResourceTable::new();
    transact(&mut table, &plan(vec![], vec![native("a"), native("b")]));
    let lowered = plan(vec![], vec![native("a"), native("c")]);
    let transaction = table.transaction(&lowered.resources).unwrap();
    table.begin(&transaction, &lowered.resources);
    // The failed part's addition is dropped as soon as the host answers, before the publication.
    let (answered, _) = table
        .answer(
            transaction.revision,
            &dd_out(),
            PartAnswer::Failed("no client".to_string()),
        )
        .unwrap();
    assert!(answered);
    assert_eq!(states(&table).len(), 2);
    let completed = table.complete().unwrap();
    assert!(completed.left_out);
    assert_eq!(
        completed.failures,
        vec![(reference("c", 2, 3), "no client".to_string())]
    );
    assert_eq!(completed.publish[0].native_set, vec![reference("a", 0, 1)]);
    assert_eq!(completed.publish[0].withdrawn, BTreeSet::from([id("b")]));
    assert_eq!(completed.released.recipients[&dd_out()], vec![reference("b", 1, 2)]);
    assert_eq!(
        states(&table),
        vec![
            ("a".to_string(), 0, 1, RecipientState::Live),
            ("b".to_string(), 1, 2, RecipientState::Draining),
        ]
    );
    // The addition is tried again by the next transaction.
    let retried = table.transaction(&lowered.resources).unwrap();
    assert_eq!(retried.prepare[0].create[0].recipient, reference("c", 3, 4));
}

#[test]
fn a_superseded_transaction_keeps_what_the_newer_one_prepares_and_drops_the_rest() {
    let mut table = ResourceTable::with_bounds(Arc::new(Bounds));
    let older = plan(vec![], vec![native("a"), native("b")]);
    let first = table.transaction(&older.resources).unwrap();
    table.begin(&first, &older.resources);
    // A host's answer after the transaction was superseded is ignored.
    let newer = plan(vec![], vec![native("b"), native("c")]);
    let second = table.transaction(&newer.resources).unwrap();
    assert_eq!(second.revision, ResourcesRevision::new(2));
    assert_eq!(
        second.prepare[0]
            .create
            .iter()
            .map(|create| create.recipient.clone())
            .collect::<Vec<_>>(),
        vec![reference("b", 1, 2), reference("c", 2, 3)],
        "the newer transaction creates `b` again under the reference the older one prepares"
    );
    let (revision, hosts, _) = table.begin(&second, &newer.resources).unwrap();
    assert_eq!((revision, hosts), (ResourcesRevision::new(1), vec![dd_out()]));
    assert!(table.answer(first.revision, &dd_out(), PartAnswer::Prepared).is_none());
    assert_eq!(
        states(&table),
        vec![
            ("b".to_string(), 1, 2, RecipientState::Preparing),
            ("c".to_string(), 2, 3, RecipientState::Preparing),
        ]
    );
    // The idle host's queue credit was listed with the first transaction's first recipient, once.
    assert_eq!(
        table
            .credits()
            .map(|(credit, bytes)| (credit.clone(), bytes))
            .collect::<Vec<_>>(),
        vec![(CreditKey::HostQueue(dd_out()), 1_000)]
    );
}

#[test]
fn a_timed_out_host_is_aborted_and_publishes_without_its_additions() {
    let mut table = ResourceTable::new();
    let lowered = plan(vec![], vec![native("a"), recipient_on("mrf_dd_out", "m", false, false)]);
    let transaction = table.transaction(&lowered.resources).unwrap();
    assert_eq!(transaction.prepare.len(), 2);
    table.begin(&transaction, &lowered.resources);
    assert_eq!(
        table
            .answer(transaction.revision, &dd_out(), PartAnswer::Prepared)
            .map(|(answered, _)| answered),
        Some(false)
    );
    let completed = table.complete().unwrap();
    assert_eq!(completed.aborted, vec![mrf()]);
    assert!(completed.left_out);
    assert_eq!(
        completed
            .publish
            .iter()
            .map(|publish| (publish.host.clone(), publish.native_set.len()))
            .collect::<Vec<_>>(),
        vec![(dd_out(), 1), (mrf(), 0)]
    );
    assert_eq!(states(&table), vec![("a".to_string(), 0, 1, RecipientState::Live)]);
}

#[test]
fn a_failed_recipients_next_incarnation_takes_its_slot_after_its_backoff_and_older_incarnations_are_ignored() {
    let mut table = ResourceTable::new();
    let lowered = plan(vec![output(MAIN, &["a"])], vec![native("a")]);
    install(&mut table, generation(1), &lowered);
    transact(&mut table, &lowered);
    assert!(table.recipient_failed(&reference("a", 0, 1)).is_some());
    assert!(
        table.recipient_failed(&reference("a", 0, 1)).is_none(),
        "an incarnation fails once"
    );
    assert_eq!(states(&table)[0].3, RecipientState::Failed);
    assert_eq!(table.failed(&reference("a", 0, 1)), Some((dd_out(), 1)));
    // Until its backoff passes, it stays in the native set its host routes over, where what is sent to it is dropped
    // and counted at its slot, as natively: nothing needs publishing.
    assert_eq!(table.transaction(&lowered.resources), None);
    assert_eq!(
        table
            .published(&dd_out())
            .map(|(_, publish)| publish.native_set.clone()),
        Some(vec![reference("a", 0, 1)])
    );
    // Once its backoff passed, a transaction prepares its next incarnation into the same slot.
    table.failure_due(&dd_out(), &reference("a", 0, 1).id);
    assert_eq!(table.failed(&reference("a", 0, 1)), None, "due");
    let transaction = table.transaction(&lowered.resources).unwrap();
    assert_eq!(transaction.prepare[0].create[0].recipient, reference("a", 0, 2));
    // A failure of the next incarnation's preparation returns the slot to the failed incarnation.
    table.begin(&transaction, &lowered.resources);
    table.answer(
        transaction.revision,
        &dd_out(),
        PartAnswer::Failed("disk full".to_string()),
    );
    let completed = table.complete().unwrap();
    assert_eq!(
        completed.publish[0].native_set,
        vec![reference("a", 0, 1)],
        "the failed incarnation keeps its place in the native set"
    );
    // Its backoff passed already: the next transaction prepares another incarnation.
    assert_eq!(states(&table), vec![("a".to_string(), 0, 1, RecipientState::Failed)]);
    let completed = transact(&mut table, &lowered);
    assert_eq!(completed.publish[0].native_set, vec![reference("a", 0, 3)]);
    assert_eq!(states(&table), vec![("a".to_string(), 0, 3, RecipientState::Live)]);
    assert!(table.recipient_failed(&reference("a", 0, 1)).is_none());
    assert!(table.recipient_retired(&reference("a", 0, 2)).is_none());
}

#[test]
fn a_reopen_answered_exited_waits_for_the_exit_before_the_next_incarnation() {
    let mut table = ResourceTable::new();
    let candidate = plan(vec![output(MAIN, &["a"])], vec![native("a")]);
    install(&mut table, generation(1), &candidate);
    install(&mut table, generation(2), &plan(vec![output(GROUP, &[])], vec![]));
    let lease = table.retire(generation(1)).leases[&dd_out()][0].clone();
    table.lease_released(&dd_out(), &lease).unwrap();

    let planned = table.plan(&candidate);
    assert_eq!(planned.hosts[0].reopen[0].recipient, reference("a", 0, 1));
    table.record(generation(3), &candidate, &planned, BTreeMap::new());
    table.reopen_exited(
        Creator::Generation(generation(3)),
        &[reference("a", 0, 1)],
        &mut Released::default(),
    );
    table.abort(generation(3));
    assert_eq!(table.waiting_for(&candidate), vec![reference("a", 0, 1)]);
    assert_eq!(states(&table), vec![("a".to_string(), 0, 1, RecipientState::Exiting)]);

    table.recipient_retired(&reference("a", 0, 1)).unwrap();
    assert!(table.waiting_for(&candidate).is_empty());
    let planned = table.plan(&candidate);
    assert_eq!(planned.hosts[0].create[0].recipient, reference("a", 1, 2));
}

#[test]
fn credits_are_listed_for_what_a_preparation_adds_and_each_is_released_once_however_it_ends() {
    let mut table = ResourceTable::with_bounds(Arc::new(Bounds));
    let candidate = plan(
        vec![
            output(MAIN, &["d"]),
            output_on("mrf_metrics_encode", "mrf_dd_out", &["m"], json!("native")),
        ],
        vec![declared("d"), recipient_on("mrf_dd_out", "m", false, false)],
    );
    let planned = table.plan(&candidate);
    let main = lease_of(&planned, MAIN);
    let failover = lease_of(&planned, "mrf_metrics_encode");
    let d = reference("d", 0, 1);
    // Each new lease, the declared recipient's own bound, and each idle host's queue, once.
    assert_eq!(
        planned.admitted,
        vec![
            (CreditKey::Recipient(dd_out(), d.clone()), 10),
            (CreditKey::HostQueue(dd_out()), 1_000),
            (CreditKey::HostQueue(mrf()), 1_000),
            (CreditKey::Lease(dd_out(), main, 0), 100),
            (CreditKey::Lease(mrf(), failover, 0), 100),
        ]
    );
    let kernels = BTreeMap::from([(SlotId::new(1), (Vec::new(), 7)), (SlotId::new(2), (Vec::new(), 0))]);
    let admitted = table.record(generation(1), &candidate, &planned, kernels);
    assert_eq!(
        admitted.last(),
        Some(&(CreditKey::Instances(generation(1), SlotId::new(1)), 7))
    );
    table.commit(generation(1));

    // The generation retires: its kernel instances return at once, and its leases drain.
    let released = table.retire(generation(1));
    assert_eq!(
        released.credits,
        vec![CreditKey::Instances(generation(1), SlotId::new(1))]
    );
    let mut credits = Vec::new();
    for (host, leases) in released.leases {
        for lease in leases {
            let released = table.lease_released(&host, &lease).unwrap();
            credits.extend(released.credits);
            for recipient in released.recipients.into_values().flatten() {
                credits.extend(table.recipient_retired(&recipient).unwrap().credits);
            }
        }
    }
    assert_eq!(
        credits,
        vec![
            CreditKey::Lease(dd_out(), main, 0),
            CreditKey::Recipient(dd_out(), d),
            CreditKey::HostQueue(dd_out()),
            CreditKey::Lease(mrf(), failover, 0),
            CreditKey::HostQueue(mrf()),
        ]
    );
    assert_eq!(table.credits().count(), 0, "every credit returned");
}

#[test]
fn a_failed_recipient_returns_its_credit_at_the_failure_and_its_next_incarnation_holds_its_own() {
    let mut table = ResourceTable::with_bounds(Arc::new(Bounds));
    let candidate = plan(vec![output(MAIN, &["d"])], vec![declared("d")]);
    install(&mut table, generation(1), &candidate);
    let released = table.recipient_failed(&reference("d", 0, 1)).unwrap();
    assert_eq!(
        released.credits,
        vec![CreditKey::Recipient(dd_out(), reference("d", 0, 1))]
    );
    table.failure_due(&dd_out(), &reference("d", 0, 1).id);
    // A declared recipient no native set names is recovered by a transaction too, since a generation uses it.
    let transaction = table.transaction(&candidate.resources).unwrap();
    assert_eq!(transaction.prepare[0].create[0].recipient, reference("d", 0, 2));
    assert_eq!(
        transaction.admitted,
        vec![(CreditKey::Recipient(dd_out(), reference("d", 0, 2)), 10)]
    );
}

#[test]
fn lease_keys_cover_the_binding_the_accepted_records_and_each_recipients_protocol_but_not_provenance() {
    let key = |output: Value, recipients: Vec<Value>| {
        let decl = serde_json::from_value::<OutputDecl>(output).unwrap();
        LeaseKey::of(&decl, &plan(vec![], recipients).resources)
            .unwrap()
            .binding
    };
    let base = key(output(MAIN, &["a"]), vec![native("a")]);
    let declared_by = json!({"declared": {"revision": 9, "hash": HASH}});
    assert_eq!(
        key(output_on(MAIN, "dd_out", &["a"], declared_by), vec![native("a")]),
        base
    );
    assert_ne!(key(output(MAIN, &["a", "b"]), vec![native("a"), native("b")]), base);
    assert_ne!(
        key(output(MAIN, &["a"]), vec![recipient_on("dd_out", "a", false, true)]),
        base,
        "a recipient's V3 acceptance changes the variants the lease produces"
    );
    let mut other_host = output(MAIN, &["a"]);
    other_host["binding"]["host"] = json!("mrf_dd_out");
    assert_ne!(key(other_host, vec![native("a")]), base);
    let unbound = serde_json::from_value::<OutputDecl>(json!({
        "id": "dsd_client_telemetry", "accepts": {"kinds": "all", "timestamps": "mixed"},
        "fingerprint": format!("sha256:{}", "0".repeat(64))
    }))
    .unwrap();
    assert_eq!(LeaseKey::of(&unbound, &Resources::default()), None);
}

#[test]
fn an_output_without_a_binding_keeps_the_lease_it_was_given_and_no_host_runs_it() {
    let mut table = ResourceTable::new();
    let telemetry = json!({
        "id": "dsd_client_telemetry", "accepts": {"kinds": "all", "timestamps": "mixed"},
        "fingerprint": format!("sha256:{}", "0".repeat(64))
    });
    let first = install(&mut table, generation(1), &plan(vec![telemetry.clone()], vec![]));
    assert!(first.hosts.is_empty());
    let lease = lease_of(&first, "dsd_client_telemetry");
    let second = install(
        &mut table,
        generation(2),
        &plan(vec![telemetry, output(MAIN, &[])], vec![]),
    );
    assert_eq!(lease_of(&second, "dsd_client_telemetry"), lease);
}

#[test]
fn identifiers_and_slots_are_allocated_in_rotation_over_all_65536_values() {
    let values = rotation(65_535).take(3).collect::<Vec<_>>();
    assert_eq!(values, vec![65_535, 0, 1]);
    assert_eq!(rotation(7).count(), 65_536);
    assert_eq!(rotation(65_536).next(), Some(0), "the cursor wraps");
}

#[test]
fn credits_name_their_resource_in_words() {
    let host = dd_out();
    let recipient = RecipientRef {
        slot: RecipientSlot::new(2),
        id: RecipientId::new("dd_out/declared/team_d/https://team-d.example.com").unwrap(),
        incarnation: 5,
    };
    let cases = [
        (
            CreditKey::Lease(host.clone(), EntryId::new(3), 1),
            "lease 3 (sequence 1) of the encode host of `dd_out`",
        ),
        (
            CreditKey::Recipient(host.clone(), recipient),
            "recipient `dd_out/declared/team_d/https://team-d.example.com` (slot 2, incarnation 5) of delivery \
             host `dd_out`",
        ),
        (CreditKey::HostQueue(host), "the queue of delivery host `dd_out`"),
        (
            CreditKey::Instances(GenerationId::new(4), SlotId::new(1)),
            "the new kernel instances of generation 4 in slot 1",
        ),
    ];
    for (credit, words) in cases {
        assert_eq!(credit.to_string(), words);
    }
}
