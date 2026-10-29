//! A stand-in Docker Engine API serving `docker exec` for unit tests.

use std::collections::HashMap;
use std::future::{pending, Future};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bollard::{Docker, API_DEFAULT_VERSION};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{JoinHandle, JoinSet};

use super::target_exec::TEST_DOCKER;

/// How the fake Docker runs one exec.
#[derive(Clone)]
pub(crate) enum Exec {
    /// Docker never answers the request to start the command.
    Stalled,
    /// Docker starts the command, but its output stream never finishes.
    Running,
    /// The command runs for `duration`, prints `stdout`, and exits with `exit_code`.
    Exits {
        duration: Duration,
        stdout: String,
        exit_code: i64,
    },
}

/// Chooses how to run the exec with the given index and command.
type ExecPlan = dyn Fn(usize, &[String]) -> Exec + Send + Sync;

#[derive(Default)]
struct State {
    /// The command of each created exec, by index.
    commands: Vec<Vec<String>>,
    /// How each created exec runs, by index.
    execs: Vec<Exec>,
}

/// A local Docker Engine API that serves `docker exec` requests. It stops when dropped.
///
/// [`docker_exec`](super::docker_exec) calls inside [`FakeDocker::scope`] use it instead of the real Docker.
pub(crate) struct FakeDocker {
    docker: Docker,
    state: Arc<Mutex<State>>,
    task: JoinHandle<()>,
}

impl FakeDocker {
    /// Serves execs as `plan` chooses, answering each Docker API request after `latency`.
    pub(crate) async fn start(
        latency: Duration, plan: impl Fn(usize, &[String]) -> Exec + Send + Sync + 'static,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("should bind an ephemeral port");
        let port = listener.local_addr().expect("should have a local address").port();
        let docker = Docker::connect_with_http(&format!("tcp://127.0.0.1:{port}"), 120, API_DEFAULT_VERSION)
            .expect("should build a Docker client");

        let state = Arc::new(Mutex::new(State::default()));
        let plan: Arc<ExecPlan> = Arc::new(plan);
        let server_state = state.clone();
        let task = tokio::spawn(async move {
            let mut requests = JoinSet::new();
            loop {
                tokio::select! {
                    connection = listener.accept() => {
                        let Ok((stream, _)) = connection else { break };
                        requests.spawn(respond(stream, latency, server_state.clone(), plan.clone()));
                    }
                    Some(result) = requests.join_next(), if !requests.is_empty() => {
                        result.expect("fake Docker request should not panic");
                    }
                }
            }
        });
        Self { docker, state, task }
    }

    /// Runs `future` with its Docker execs served by this fake.
    pub(crate) async fn scope<F: Future>(&self, future: F) -> F::Output {
        TEST_DOCKER.scope(self.docker.clone(), future).await
    }

    /// Returns the command of every exec created so far.
    pub(crate) fn launched(&self) -> Vec<Vec<String>> {
        self.state
            .lock()
            .expect("state lock should not be poisoned")
            .commands
            .clone()
    }
}

impl Drop for FakeDocker {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Returns the `--max-time` a curl command was given.
pub(crate) fn curl_max_time(command: &[String]) -> Duration {
    let words = command
        .iter()
        .flat_map(|arg| arg.split_whitespace())
        .collect::<Vec<_>>();
    let seconds = words
        .iter()
        .position(|word| *word == "--max-time")
        .and_then(|index| words.get(index + 1))
        .unwrap_or_else(|| panic!("curl should bound itself: {command:?}"));
    Duration::from_secs_f64(seconds.parse().expect("--max-time should be a number of seconds"))
}

async fn respond(mut stream: TcpStream, latency: Duration, state: Arc<Mutex<State>>, plan: Arc<ExecPlan>) {
    let Some((method, path, body)) = read_request(&mut stream).await else {
        return;
    };
    tokio::time::sleep(latency).await;

    if method == "POST" && path.contains("/containers/") && path.ends_with("/exec") {
        let request: serde_json::Value = serde_json::from_slice(&body).expect("create_exec body should be JSON");
        let command = serde_json::from_value::<Vec<String>>(request["Cmd"].clone()).expect("exec should have a Cmd");
        let id = {
            let mut state = state.lock().expect("state lock should not be poisoned");
            let id = state.commands.len();
            state.execs.push(plan(id, &command));
            state.commands.push(command);
            id
        };
        write_json(&mut stream, "201 Created", &serde_json::json!({ "Id": id.to_string() })).await;
    } else if method == "POST" && path.ends_with("/start") {
        let execution = exec(&state, &path);
        if matches!(execution, Exec::Stalled) {
            let _stream = stream;
            return pending().await;
        }
        let upgraded = "HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n";
        if stream.write_all(upgraded.as_bytes()).await.is_err() {
            return;
        }
        let Exec::Exits { duration, stdout, .. } = execution else {
            let _stream = stream;
            return pending().await;
        };
        tokio::time::sleep(duration).await;
        if !stdout.is_empty() {
            // Docker multiplexes a non-TTY exec's streams: stream 1 is stdout, followed by a big-endian length.
            let mut frame = vec![1, 0, 0, 0];
            frame.extend_from_slice(
                &u32::try_from(stdout.len())
                    .expect("output should be short")
                    .to_be_bytes(),
            );
            frame.extend_from_slice(stdout.as_bytes());
            let _ = stream.write_all(&frame).await;
        }
        let _ = stream.shutdown().await;
    } else if method == "GET" && path.contains("/exec/") && path.ends_with("/json") {
        let Exec::Exits { exit_code, .. } = exec(&state, &path) else {
            panic!("a stalled exec should never be inspected");
        };
        let inspect = serde_json::json!({ "Running": false, "ExitCode": exit_code });
        write_json(&mut stream, "200 OK", &inspect).await;
    } else {
        panic!("unexpected Docker API request: {method} {path}");
    }
}

/// Returns the plan of the exec named in `path`, which looks like `/v1.xx/exec/{id}/...`.
fn exec(state: &Mutex<State>, path: &str) -> Exec {
    let id = path
        .split('/')
        .skip_while(|segment| *segment != "exec")
        .nth(1)
        .and_then(|id| id.parse::<usize>().ok())
        .unwrap_or_else(|| panic!("request should name an exec: {path}"));
    state.lock().expect("state lock should not be poisoned").execs[id].clone()
}

/// Reads one request, returning its method, path, and body.
async fn read_request(stream: &mut TcpStream) -> Option<(String, String, Vec<u8>)> {
    let mut request = Vec::new();
    let mut buffer = [0u8; 4096];
    let header_end = loop {
        if let Some(index) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    };

    let head = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect::<HashMap<_, _>>();
    let content_length = headers
        .get("content-length")
        .and_then(|length| length.parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => request.extend_from_slice(&buffer[..read]),
        }
    }

    let mut request_line = head.split_whitespace();
    let method = request_line.next()?.to_string();
    let path = request_line.next()?.to_string();
    Some((method, path, request[header_end..header_end + content_length].to_vec()))
}

async fn write_json(stream: &mut TcpStream, status: &str, body: &serde_json::Value) {
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
