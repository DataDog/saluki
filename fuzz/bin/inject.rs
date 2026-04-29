use std::net::UdpSocket;
use std::path::PathBuf;

use argh::FromArgs;

#[derive(FromArgs)]
#[argh(
    description = "Replay a corpus file line-by-line over UDP.",
    help_triggers("-h", "--help", "help")
)]
pub struct Cli {
    /// path to the corpus file
    #[argh(option, short = 'c', long = "corpus")]
    pub corpus: PathBuf,
    /// ADP port to send to
    #[argh(option, long = "port", default = "8125")]
    pub port: u16,
}

fn main() {
    let cli: Cli = argh::from_env();

    let content = std::fs::read_to_string(&cli.corpus)
        .unwrap_or_else(|e| panic!("failed to read corpus {:?}: {}", cli.corpus, e));

    let lines: Vec<String> = content.lines().filter(|l| !l.is_empty()).map(String::from).collect();

    let socket = UdpSocket::bind("0.0.0.0:0").expect("failed to bind UDP socket");
    socket
        .connect(("127.0.0.1", cli.port))
        .unwrap_or_else(|e| panic!("failed to connect to port {}: {}", cli.port, e));

    for line in &lines {
        if let Err(e) = socket.send(line.as_bytes()) {
            eprintln!("send error: {e}");
        }
    }
}
