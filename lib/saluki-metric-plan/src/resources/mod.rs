//! Output fingerprints: what an output's declaration says it delivers to.

use serde_json::json;

use crate::{
    canonical::PlanHash,
    ir::{HostId, Selector},
    model::PortType,
};

/// The version of output fingerprints that [`fingerprint`] computes.
pub const FINGERPRINT_VERSION: u32 = 2;

/// Returns an output's fingerprint, version [`FINGERPRINT_VERSION`]: the digest of `{"v": 2, "host": <delivery host>,
/// "accepts": <port type>, "selector": <selector>}`.
///
/// A fingerprint covers what an output's declaration says it delivers to: for a role, whatever native configuration
/// resolves for it, so recipients added or removed under a role change nothing; for a routing group, its key; for an
/// implied output, its recipient list. Encoder parameters, delivery policies, recipient settings, and credentials are
/// never part of it.
pub fn fingerprint(host: &HostId, accepts: PortType, selector: &Selector) -> PlanHash {
    let document = json!({
        "v": FINGERPRINT_VERSION,
        "host": host,
        "accepts": accepts,
        "selector": selector,
    });
    // The document holds strings and one small integer, so it always has a canonical form.
    PlanHash::of_value(&document).expect("fingerprint documents have a canonical form")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ir::{RecipientId, RoleName},
        model::{Kind, KindSet, TimestampForm},
    };

    #[test]
    fn fingerprints_are_the_digests_of_their_hand_written_canonical_documents() {
        let dd_out = HostId::new("dd_out").unwrap();
        let team_b = RecipientId::new("dd_out/additional/0/http://127.0.0.1:17102").unwrap();
        let primary = RecipientId::new("dd_out/primary/0/https://app.datadoghq.com").unwrap();
        let group = "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        let scalars = PortType {
            kinds: KindSet::of(&[Kind::Counter, Kind::Rate, Kind::Gauge, Kind::Set]),
            timestamps: TimestampForm::Mixed,
        };
        let cases = [
            // {"accepts":{"kinds":"all","timestamps":"mixed"},"host":"dd_out","selector":{"role":"main"},"v":2}
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Role(RoleName::new("main").unwrap())),
                "sha256:35b6c7483c20787a99a97a2a2bb2684fd1a0a8f0315e5bf73fad5b48b74595f7",
            ),
            // {"accepts":{...},"host":"dd_out","selector":{"group":"sha256:ba78...15ad"},"v":2}
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Group(group.parse().unwrap())),
                "sha256:cb8e7cce9492fb8586c4c8eb5ba80f900e475c06d6a5eaf601386c4df7daedb9",
            ),
            // {"accepts":{...},"host":"mrf_dd_out","selector":{"role":"multi_region_failover"},"v":2}
            (
                fingerprint(
                    &HostId::new("mrf_dd_out").unwrap(),
                    PortType::ANY,
                    &Selector::Role(RoleName::new("multi_region_failover").unwrap()),
                ),
                "sha256:0ff4cb7e0e1316c414965f9ca7858095cb8feff59bd696171455cbe959c77c08",
            ),
            // {"accepts":{"kinds":["counter","rate","gauge","set"],"timestamps":"mixed"},"host":"cluster_agent_out",
            //  "selector":{"role":"autoscaling_failover"},"v":2}
            (
                fingerprint(
                    &HostId::new("cluster_agent_out").unwrap(),
                    scalars,
                    &Selector::Role(RoleName::new("autoscaling_failover").unwrap()),
                ),
                "sha256:608361af9ff1be71390572e16f40425d91adeab874b3f89d17435918832514ff",
            ),
            // {"accepts":{...},"host":"dd_out",
            //  "selector":{"recipients":["dd_out/additional/0/http://127.0.0.1:17102"]},"v":2}
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Recipients(vec![team_b.clone()])),
                "sha256:078861188ff436606d2e89dae13fb835b363b844df9d556384fcd72127f85045",
            ),
            // The same with the primary's recipient added.
            (
                fingerprint(&dd_out, PortType::ANY, &Selector::Recipients(vec![team_b, primary])),
                "sha256:8497cb73d0faff2547e4ae2c0f4727722255cdbf0b3445741239f1916b2f9069",
            ),
        ];
        for (computed, expected) in cases {
            assert_eq!(computed.to_string(), expected);
        }
    }
}
