use std::thread;

use saluki_metrics::{static_metrics, Counter};

#[static_metrics(prefix = "concurrent", labels(id))]
#[derive(Clone)]
struct Metrics {
    #[metric(mapped(shard))]
    ops_total: Counter,
}

fn main() {
    let metrics = Metrics::new("x".to_string());

    // Exercise concurrent lookups/registrations across threads with overlapping and unique keys. The struct is
    // cheaply cloned into each thread and all clones share one handle cache.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let metrics = metrics.clone();
        handles.push(thread::spawn(move || {
            for i in 0..1_000 {
                let shard = (i % 4).to_string();
                metrics.ops_total(&shard).increment(1);
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }
}
