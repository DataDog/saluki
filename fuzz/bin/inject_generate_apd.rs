#![feature(thread_sleep_until)]
#![feature(slice_split_once)]
use std::time::{Duration, Instant};
use std::{collections::BTreeMap, net::UdpSocket};
use std::path::PathBuf;
use barkus_ebnf;
use barkus_core;
use argh::FromArgs;
use rand::SeedableRng;

#[derive(FromArgs)]
#[argh(
    description = "Replay a grammar file line-by-line over UDP.",
    help_triggers("-h", "--help", "help")
)]
pub struct Cli {
    /// path to the grammar file
    #[argh(option, long = "grammar", default = "PathBuf::from(\"fuzz/dogstatsd_offset.ebnf\")")]
    pub grammar: PathBuf,
    /// ADP port to send to
    #[argh(option, long = "port", default = "8125")]
    pub port: u16,
    /// message count
    #[argh(option, long = "count", default = "1000")]
    pub count: u64,
}

fn main() {
    let cli: Cli = argh::from_env();

    let grammar_src = std::fs::read_to_string(cli.grammar).unwrap();
    let grammar_ir = barkus_ebnf::compile(&grammar_src)
        .expect("grammar compile error");

    let mut profile = barkus_core::profile::Profile::default();
    profile.max_depth = 15;
    profile.max_total_nodes = 5_000;

    let seed = 42u64;
    let mut rng = rand::rngs::SmallRng::seed_from_u64(seed);

    let mut sequence: BTreeMap<u64, Vec<String>> = BTreeMap::new();

    for _packet_ix in 0..cli.count {
            match barkus_core::generate::generate(&grammar_ir, &profile, &mut rng) {
                Ok((ast, _, _)) => {
                    let bytes = ast.serialize();
                    let( offset, message) = bytes.split_once(|x| *x==b';').unwrap();
                    let offset_ts: u64 =  std::str::from_utf8(&offset).expect("invalid utf8").parse().expect("should be a number");
                    sequence.entry(offset_ts).or_default().push(std::str::from_utf8(message).unwrap().into());
                }
                Err(_) => {
                }
        }
    }

    let start = Instant::now();
    let socket = UdpSocket::bind("0.0.0.0:0").expect("failed to bind UDP socket");
    socket
        .connect(("127.0.0.1", cli.port))
        .unwrap_or_else(|e| panic!("failed to connect to port {}: {}", cli.port, e));

    for (offset, val) in sequence {
        let target = start + Duration::from_millis(10 * offset);
        std::thread::sleep_until(target);
        println!("{}", offset);
        for item in val {
            println!("{item}");
            if let Err(e) = socket.send(item.as_bytes()) {
                eprintln!("send error: {e}");
            }
        }
    }
}
