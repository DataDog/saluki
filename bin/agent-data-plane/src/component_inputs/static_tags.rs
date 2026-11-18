use agent_data_plane_config::shared;
use tracing::warn;

/// Resolves tags which must be attached directly to Fargate-sidecar telemetry.
///
/// These tags are necessary because Fargate sidecars do not have host tags for the backend to associate with their
/// telemetry.
pub(crate) fn resolve_static_tags(
    static_tags: &shared::StaticTagSettings, global_tags: &shared::GlobalTags, is_ecs_fargate: bool,
) -> Vec<String> {
    let mut tags = Vec::new();

    if !static_tags.provider_kind.is_empty() {
        tags.push(format!("provider_kind:{}", static_tags.provider_kind));
    }

    if !is_ecs_fargate && !static_tags.eks_fargate {
        return tags;
    }

    tags.extend(global_tags.tags.iter().cloned());
    tags.extend(global_tags.extra_tags.iter().cloned());

    if !static_tags.eks_fargate {
        return tags;
    }

    if !static_tags.kubernetes_kubelet_nodename.is_empty() {
        tags.push(format!("eks_fargate_node:{}", static_tags.kubernetes_kubelet_nodename));
    } else {
        warn!(
            "Tag 'eks_fargate_node' will be missing from Fargate telemetry due to missing configuration data. Set \
             'kubernetes_kubelet_nodename' in the Agent configuration."
        );
    }

    if !tags.iter().any(|tag| tag.starts_with("kube_cluster_name:")) {
        if static_tags.cluster_name.is_empty() {
            warn!("Couldn't build the 'kube_cluster_name' tag: cluster_name is not configured.");
        } else {
            tags.push(format!("kube_cluster_name:{}", static_tags.cluster_name));
        }
    }

    tags.push("kube_distribution:eks".to_string());
    tags
}

#[cfg(test)]
mod tests {
    use agent_data_plane_config::shared;

    use super::resolve_static_tags;

    #[test]
    fn static_metric_tags_match_provider_and_eks_fargate_order() {
        let static_tags = shared::StaticTagSettings {
            provider_kind: "autopilot".to_string(),
            eks_fargate: true,
            kubernetes_kubelet_nodename: "node-a".to_string(),
            cluster_name: "configured-cluster".to_string(),
        };
        let global_tags = shared::GlobalTags {
            tags: vec!["kube_cluster_name:manual-cluster".to_string(), "env:prod".to_string()],
            extra_tags: vec!["team:metrics".to_string()],
            ..Default::default()
        };

        let tags = resolve_static_tags(&static_tags, &global_tags, false);

        assert_eq!(
            tags,
            [
                "provider_kind:autopilot",
                "kube_cluster_name:manual-cluster",
                "env:prod",
                "team:metrics",
                "eks_fargate_node:node-a",
                "kube_distribution:eks",
            ]
        );
    }

    #[test]
    fn ecs_fargate_static_tags_include_global_tags() {
        let global_tags = shared::GlobalTags {
            tags: vec!["env:prod".to_string()],
            extra_tags: vec!["team:metrics".to_string()],
            ..Default::default()
        };

        let tags = resolve_static_tags(&shared::StaticTagSettings::default(), &global_tags, true);

        assert_eq!(tags, ["env:prod", "team:metrics"]);
    }
}
