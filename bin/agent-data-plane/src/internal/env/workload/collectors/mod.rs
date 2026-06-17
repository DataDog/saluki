//! Remote Datadog Agent workload metadata collectors.

pub mod tagger;
pub mod workloadmeta;

use datadog_protos::agent::{EntityId as ProtoEntityId, WorkloadmetaEntityId, WorkloadmetaKind};
use saluki_env::workload::EntityId;

pub(super) fn entity_id_from_tagger(id: ProtoEntityId) -> Option<EntityId> {
    let prefix = id.prefix.trim_end_matches("://");
    match prefix {
        "global" | "system" => Some(EntityId::Global),
        "kubernetes_pod_uid" | "kube_pod_uid" | "pod_uid" => EntityId::from_pod_uid(id.uid),
        "container_id" | "container" => Some(EntityId::Container(id.uid.into())),
        "container_inode" => id.uid.parse().ok().map(EntityId::ContainerInode),
        "container_pid" => id.uid.parse().ok().map(EntityId::ContainerPid),
        _ => parse_display_entity_id(&format!("{}://{}", id.prefix, id.uid)),
    }
}

pub(super) fn entity_id_from_workloadmeta(id: Option<WorkloadmetaEntityId>) -> Option<EntityId> {
    let id = id?;
    match WorkloadmetaKind::try_from(id.kind).ok()? {
        WorkloadmetaKind::Container => Some(EntityId::Container(id.id.into())),
        WorkloadmetaKind::KubernetesPod => EntityId::from_pod_uid(id.id),
        _ => None,
    }
}

fn parse_display_entity_id(raw: &str) -> Option<EntityId> {
    let (prefix, value) = raw.split_once("://")?;
    match prefix {
        "system" if value == "global" => Some(EntityId::Global),
        "kubernetes_pod_uid" => EntityId::from_pod_uid(value),
        "container_id" => Some(EntityId::Container(value.into())),
        "container_inode" => value.parse().ok().map(EntityId::ContainerInode),
        "container_pid" => value.parse().ok().map(EntityId::ContainerPid),
        _ => None,
    }
}
