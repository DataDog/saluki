use std::collections::BTreeMap;
use std::net::UdpSocket;
use std::process::Command;
use std::time::{Duration, Instant};

use argh::FromArgs;

#[derive(FromArgs)]
#[argh(
    description = "Generate DogStatsD traffic via barkus-cli and replay over UDP with timing.",
    help_triggers("-h", "--help", "help")
)]
pub struct Cli {
    /// ADP port to send to
    #[argh(option, long = "port", default = "8125")]
    pub port: u16,
    /// number of payloads to generate
    #[argh(option, long = "count", default = "1000")]
    pub count: usize,
}

fn main() {
    let cli: Cli = argh::from_env();

    let output = Command::new("../barkus/target/debug/barkus-cli")
        .args(["generate", "fuzz/dogstatsd_offset.ebnf", "--count", &cli.count.to_string()])
        .output()
        .expect("failed to run barkus-cli");

    if !output.status.success() {
        eprintln!("barkus-cli error: {}", String::from_utf8_lossy(&output.stderr));
        std::process::exit(1);
    }

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Group lines by timestamp (digits before ';'); send only the metric part after ';'
    let mut groups: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for line in stdout.lines() {
        if let Some(semi) = line.find(';') {
            if let Ok(ts) = line[..semi].parse::<u64>() {
                groups.entry(ts).or_default().push(line[semi + 1..].to_string());
            }
        }
    }

    let socket = UdpSocket::bind("0.0.0.0:0").expect("failed to bind UDP socket");
    socket
        .connect(("127.0.0.1", cli.port))
        .unwrap_or_else(|e| panic!("failed to connect to port {}: {}", cli.port, e));

    let start = Instant::now();
    for (ts, packets) in &groups {
        // timestamp X is sent at second X/10
        let target = Duration::from_millis(ts * 100);
        let elapsed = start.elapsed();
        if target > elapsed {
            std::thread::sleep(target - elapsed);
        }
        for packet in packets {
            if let Err(e) = socket.send(packet.as_bytes()) {
                eprintln!("send error: {e}");
            }
        }
    }
}
