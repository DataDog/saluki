use base64::Engine as _;
use datadog_protos::piecemeal_include::datadog::agentpayload::MetricPayloadBuilder;
use piecemeal::ScratchWriter;

fn main() {
    let mut scratch_buf = Vec::with_capacity(1024);
    let mut scratch_writer = ScratchWriter::new(&mut scratch_buf);

    let mut payload_builder = MetricPayloadBuilder::new(&mut scratch_writer);
    payload_builder
        .add_series(|series_builder| {
            series_builder.metric("metric.name")?.add_points(|pb| {
                pb.timestamp(1234567890)?.value(42.0)?;
                Ok(())
            })?;

            Ok(())
        })
        .expect("should not fail to build series");

    let mut output_buf = Vec::new();
    payload_builder
        .finish(&mut output_buf)
        .expect("should not fail to finish");

    let encoded = base64::engine::general_purpose::STANDARD.encode(&output_buf);
    println!("encoded: {}", encoded)
}
