use base64::Engine as _;
use datadog_protos::piecemeal_include::datadog::agentpayload::MetricPayloadBuilder;
use piecemeal::Writer;

fn main() {
    let mut buf = Vec::with_capacity(1024);
    let mut writer = Writer::new(&mut buf);

    let mut payload_builder = MetricPayloadBuilder::with_writer(&mut writer);
    let result = payload_builder.add_series(|series_builder| {
        series_builder
            .metric("metric.name")?
            .add_points(|pb| {
                pb.timestamp(1234567890)?
                .value(42.0)?;

                Ok(())
            })?;

        Ok(())
    });

    match result {
        Ok(_) => {
            drop(payload_builder);
            let encoded = base64::engine::general_purpose::STANDARD.encode(&buf);
            println!("encoded: {}", encoded)
        }
        Err(e) => eprintln!("Error: {}", e),
    }
}
