use std::{
    io::{Read as _, Write as _},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc::{sync_channel, Receiver, SyncSender},
    thread::sleep,
    time::Duration,
};

fn main() {
    // Read our positional arguments to get the path to the domain socket where the control plane is listening for supervisors
    // _and_ the subagent "filter", which is the specific subagent that we are interested in supervising.
    let mut args = std::env::args().skip(1);
    let socket_path = args
        .next()
        .map(PathBuf::from)
        .expect("missing control plane supervisor socket path");
    let subagent_filter = args.next().expect("missing subagents filter");
    let subagent_process_path = args.next().map(PathBuf::from).expect("missing subagent process path");
    let subagent_args = args.collect::<Vec<String>>();

    println!(
        "subagent-supervisor starting: control-plane-socket-path={} subagent-filter={} subagent-process-path={} subagent-args=[{}]",
        socket_path.display(), subagent_filter, subagent_process_path.display(), subagent_args.join(" "),
    );

    // Configure the supervisor state and run until signaled to stop.
    let mut supervisor = Supervisor::new(socket_path, subagent_filter, subagent_process_path, subagent_args);
    supervisor.run();
}

enum SupervisorOperation {
    Start,
    Stop,
    Restart,
}

enum SupervisedProcessState {
    Enabled,
    Disabled,
}

struct Supervisor {
    operations_rx: Receiver<SupervisorOperation>,
    subagent_process_path: PathBuf,
    subagent_args: Vec<String>,
    subagent_process: Option<Child>,
    supervised_process_state: SupervisedProcessState,
}

impl Supervisor {
    fn new(
        control_plane_socket_path: PathBuf, subagent_filter: String, subagent_process_path: PathBuf,
        subagent_args: Vec<String>,
    ) -> Self {
        // Spawn our control plane connection handler.
        //
        // This subscribes us to the control plane and ships operations back to the supervisor to drive supervision.
        let (operations_tx, operations_rx) = sync_channel(1);

        std::thread::spawn(move || {
            run_control_plane_connection_handler(control_plane_socket_path, subagent_filter, operations_tx)
        });

        Self {
            operations_rx,
            subagent_process_path,
            subagent_args,
            subagent_process: None,
            supervised_process_state: SupervisedProcessState::Disabled,
        }
    }

    fn run(&mut self) {
        const SUBAGENT_CHECK_INTERVAL: Duration = Duration::from_millis(500);

        // Process supervision operations.
        loop {
            match self.operations_rx.recv_timeout(SUBAGENT_CHECK_INTERVAL) {
                Ok(operation) => match operation {
                    SupervisorOperation::Start => {
                        self.supervised_process_state = SupervisedProcessState::Enabled;
                        self.start_subagent();
                    }
                    SupervisorOperation::Stop => {
                        self.supervised_process_state = SupervisedProcessState::Disabled;
                        self.stop_subagent();
                    }
                    SupervisorOperation::Restart => self.restart_subagent(),
                },
                Err(_) => {
                    // Timeout occurred, check subagent status.
                    self.check_subagent_status();
                }
            }
        }
    }

    fn start_subagent(&mut self) {
        // Subagent already running. Ignore.
        if self.subagent_process.is_some() {
            return;
        }

        println!(
            "Starting subagent: {} {}",
            self.subagent_process_path.display(),
            self.subagent_args.join(" ")
        );

        // Start the subagent process.
        let child = Command::new(&self.subagent_process_path)
            .args(&self.subagent_args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("failed to start subagent");

        self.subagent_process = Some(child);
    }

    fn stop_subagent(&mut self) {
        // Stop the subagent process.
        if let Some(mut process) = self.subagent_process.take() {
            println!("Stopping subagent...");

            process.kill().expect("failed to stop subagent");
        }
    }

    fn restart_subagent(&mut self) {
        // Restart the subagent process.
        self.stop_subagent();
        self.start_subagent();
    }

    fn check_subagent_status(&mut self) {
        // Ensure that the subagent process is running if enabled, and either s
        if let SupervisedProcessState::Enabled = self.supervised_process_state {
            let maybe_exit_status = self.subagent_process.as_mut().map(|process| process.try_wait());

            match maybe_exit_status {
                // Subagent is not running. Start it.
                None => self.start_subagent(),

                // Subagent _was_ running, but(unexpectedly) exited.
                Some(Ok(Some(status))) => {
                    println!("Subagent exited with status: {}. Restarting...", status);
                    self.subagent_process = None;
                    self.start_subagent();
                }
                // Subagent is still running.
                Some(Ok(None)) => {}

                // Failed to check subagent exit status.
                Some(Err(e)) => eprintln!("Failed to check subagent exit status: {}", e),
            }
        }
    }
}

fn run_control_plane_connection_handler(
    control_plane_socket_path: PathBuf, subagent_filter: String, operations_tx: SyncSender<SupervisorOperation>,
) {
    let subscribe_request = format!("SUBSCRIBE {}\n", subagent_filter);
    let mut read_buf = [0; 64];

    loop {
        // Connect to the control plane socket.
        let mut control_plane_socket = match UnixStream::connect(control_plane_socket_path.clone()) {
            Ok(socket) => socket,
            Err(e) => {
                eprintln!("Failed to connect to control plane socket: {}", e);
                sleep(Duration::from_secs(1));
                continue;
            }
        };

        // Subscribe to the control plane for supervision events about our target subagent.
        if let Err(e) = control_plane_socket.write_all(subscribe_request.as_bytes()) {
            eprintln!("Failed to subscribe to control plane: {}", e);
            sleep(Duration::from_secs(1));
            continue;
        }

        // Wait for the acknowledgement of our subscription.
        let ack_read_len = match control_plane_socket.read(&mut read_buf) {
            Ok(n) => n,
            Err(e) => {
                eprintln!("Failed to read acknowledgement from control plane: {}", e);
                sleep(Duration::from_secs(1));
                continue;
            }
        };

        if ack_read_len == 0 {
            eprintln!("Control plane closed connection during subscription.");
            sleep(Duration::from_secs(1));
            continue;
        }

        if &read_buf[..ack_read_len] == b"SUBSCRIBED\n" {
            println!("Subscribed to control plane for supervision events about our target subagent. Waiting...");
        } else {
            eprintln!("Unexpected acknowledgement from control plane");
            sleep(Duration::from_secs(1));
            continue;
        }

        // Wait for supervision events from the control plane.
        loop {
            let event_read_len = match control_plane_socket.read(&mut read_buf) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Failed to read event from control plane: {}", e);
                    sleep(Duration::from_secs(1));
                    continue;
                }
            };

            if event_read_len == 0 {
                eprintln!("Control plane closed connection during event reading.");
                sleep(Duration::from_secs(1));
                continue;
            }

            let supervisor_op = match &read_buf[..event_read_len] {
                b"OP_START\n" => SupervisorOperation::Start,
                b"OP_STOP\n" => SupervisorOperation::Stop,
                b"OP_RESTART\n" => SupervisorOperation::Restart,
                _ => {
                    eprintln!(
                        "Unexpected event '{}' from control plane",
                        String::from_utf8_lossy(&read_buf[..event_read_len])
                    );
                    sleep(Duration::from_secs(1));
                    continue;
                }
            };

            if let Err(_) = operations_tx.send(supervisor_op) {
                // Supervisor is gone, which means we can't ever send operations to it, so we should bail out.
                break;
            }
        }
    }
}
