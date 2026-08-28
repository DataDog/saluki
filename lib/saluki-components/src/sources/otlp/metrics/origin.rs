//! Maps an OTLP instrumentation scope name to an origin product detail identifier.

/// The product detail used when a scope is absent or does not map to a known receiver.
const ORIGIN_PRODUCT_DETAIL_UNKNOWN: u32 = 0;

/// The prefix shared by OpenTelemetry Collector receiver instrumentation scopes.
const COLLECTOR_RECEIVER_PREFIX: &str = "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/";

/// Returns the origin product detail for the given instrumentation scope name.
///
/// Returns `0` (unknown) when the scope name is empty or does not begin with the Collector receiver prefix.
/// Otherwise, extracts the receiver name—the first path segment after the prefix—and looks it up in the
/// known-receiver table.
pub fn product_detail_from_scope(scope_name: &str) -> u32 {
    product_detail_from_scope_name(scope_name)
}

fn product_detail_from_scope_name(scope_name: &str) -> u32 {
    // Strip the Collector receiver prefix, then take the first remaining path segment as the receiver name:
    //
    //   .../receiver/kubeletstatsreceiver          -> kubeletstatsreceiver
    //   .../receiver/hostmetricsreceiver/disk      -> hostmetricsreceiver
    let receiver_name = match scope_name.strip_prefix(COLLECTOR_RECEIVER_PREFIX) {
        Some(rest) => rest.split('/').next().unwrap_or(""),
        None => return ORIGIN_PRODUCT_DETAIL_UNKNOWN,
    };

    match receiver_name {
        "activedirectorydsreceiver" => 251,
        "aerospikereceiver" => 252,
        "apachereceiver" => 253,
        "apachesparkreceiver" => 254,
        "azuremonitorreceiver" => 255,
        "bigipreceiver" => 256,
        "chronyreceiver" => 257,
        "couchdbreceiver" => 258,
        "dockerstatsreceiver" => 217,
        "elasticsearchreceiver" => 218,
        "expvarreceiver" => 219,
        "filestatsreceiver" => 220,
        "flinkmetricsreceiver" => 221,
        "gitproviderreceiver" => 222,
        "haproxyreceiver" => 223,
        "hostmetricsreceiver" => 224,
        "httpcheckreceiver" => 225,
        "iisreceiver" => 226,
        "k8sclusterreceiver" => 227,
        "kafkametricsreceiver" => 228,
        "kubeletstatsreceiver" => 229,
        "memcachedreceiver" => 230,
        "mongodbatlasreceiver" => 231,
        "mongodbreceiver" => 232,
        "mysqlreceiver" => 233,
        "nginxreceiver" => 234,
        "nsxtreceiver" => 235,
        "oracledbreceiver" => 236,
        "podmanreceiver" => 521,
        "postgresqlreceiver" => 237,
        "prometheusreceiver" => 238,
        "rabbitmqreceiver" => 239,
        "redisreceiver" => 240,
        "riakreceiver" => 241,
        "saphanareceiver" => 242,
        "snmpreceiver" => 243,
        "snowflakereceiver" => 244,
        "splunkenterprisereceiver" => 245,
        "sqlserverreceiver" => 246,
        "sshcheckreceiver" => 247,
        "statsdreceiver" => 248,
        "vcenterreceiver" => 249,
        "zookeeperreceiver" => 250,
        _ => ORIGIN_PRODUCT_DETAIL_UNKNOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_receiver_scope_names_map_to_their_product_detail() {
        assert_eq!(
            product_detail_from_scope_name(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/hostmetricsreceiver"
            ),
            224
        );
        assert_eq!(
            product_detail_from_scope_name(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/kubeletstatsreceiver"
            ),
            229
        );
        assert_eq!(
            product_detail_from_scope_name(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/prometheusreceiver"
            ),
            238
        );
        assert_eq!(
            product_detail_from_scope_name(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/dockerstatsreceiver"
            ),
            217
        );
        assert_eq!(
            product_detail_from_scope_name(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/podmanreceiver"
            ),
            521
        );
    }

    #[test]
    fn sub_signal_segments_after_the_receiver_name_are_ignored() {
        assert_eq!(
            product_detail_from_scope_name(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/hostmetricsreceiver/disk"
            ),
            224
        );
    }

    #[test]
    fn unknown_receiver_name_falls_back_to_zero() {
        assert_eq!(
            product_detail_from_scope_name(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/unknownreceiver"
            ),
            0
        );
    }

    #[test]
    fn scope_name_without_collector_prefix_falls_back_to_zero() {
        assert_eq!(product_detail_from_scope_name("io.opentelemetry.myapp"), 0);
        assert_eq!(product_detail_from_scope_name(""), 0);
    }

    #[test]
    fn empty_scope_name_falls_back_to_zero() {
        assert_eq!(product_detail_from_scope(""), 0);
    }

    #[test]
    fn present_known_scope_is_resolved() {
        assert_eq!(
            product_detail_from_scope(
                "github.com/open-telemetry/opentelemetry-collector-contrib/receiver/nginxreceiver"
            ),
            234
        );
    }
}
