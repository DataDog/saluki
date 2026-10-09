//! Cross-process Checks transport throughput benchmark.

mod common;
mod worker;

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{Phase, Snapshot, Transport, Workload};
use serde::{Deserialize, Serialize};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfileKind {
    Cpu,
    Memory,
}

#[derive(Clone)]
struct Options {
    transport: Transport,
    workload: Workload,
    batch: usize,
    rate: Option<u64>,
    ring_capacity: usize,
    warmup_secs: u64,
    duration_secs: u64,
    repetitions: usize,
    output: PathBuf,
    side: Option<String>,
    profile_kind: ProfileKind,
    min_rate: u64,
    max_rate: u64,
    consumer_delay_us: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            transport: Transport::Fit,
            workload: Workload::Metrics,
            batch: 64,
            rate: Some(10_000),
            ring_capacity: 1 << 20,
            warmup_secs: 5,
            duration_secs: 30,
            repetitions: 1,
            output: PathBuf::from("bin/benchmarks/checks-ipc/results"),
            side: None,
            profile_kind: ProfileKind::Cpu,
            min_rate: 100,
            max_rate: 10_000_000,
            consumer_delay_us: 0,
        }
    }
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut options = Options::default();
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args.next().ok_or_else(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--transport" => options.transport = parse_transport(value)?,
            "--workload" => options.workload = parse_workload(value)?,
            "--batch" => options.batch = value.parse().map_err(|_| "invalid batch size")?,
            "--rate" => {
                options.rate = if value == "unlimited" {
                    None
                } else {
                    Some(value.parse().map_err(|_| "invalid rate")?)
                }
            }
            "--ring" => options.ring_capacity = value.parse().map_err(|_| "invalid ring capacity")?,
            "--warmup" => options.warmup_secs = value.parse().map_err(|_| "invalid warmup seconds")?,
            "--duration" => options.duration_secs = value.parse().map_err(|_| "invalid duration seconds")?,
            "--repetitions" => options.repetitions = value.parse().map_err(|_| "invalid repetitions")?,
            "--output" => options.output = PathBuf::from(value),
            "--side" => options.side = Some(value.clone()),
            "--kind" => {
                options.profile_kind = match value.as_str() {
                    "cpu" => ProfileKind::Cpu,
                    "memory" => ProfileKind::Memory,
                    _ => return Err("profile kind must be cpu or memory".into()),
                }
            }
            "--min-rate" => options.min_rate = value.parse().map_err(|_| "invalid minimum rate")?,
            "--max-rate" => options.max_rate = value.parse().map_err(|_| "invalid maximum rate")?,
            "--consumer-delay-us" => options.consumer_delay_us = value.parse().map_err(|_| "invalid consumer delay")?,
            _ => return Err(format!("unknown option: {flag}")),
        }
    }
    if options.batch == 0 || options.repetitions == 0 || options.duration_secs == 0 || options.warmup_secs == 0 {
        return Err("batch, repetitions, duration, and warmup must be positive".into());
    }
    if let Some(rate) = options.rate {
        let total_seconds = options
            .warmup_secs
            .checked_add(options.duration_secs)
            .ok_or("duration overflow")?;
        if rate == 0 || u128::from(rate) * u128::from(total_seconds) >= u128::from(common::MAX_SEQUENCE) {
            return Err("rate must be positive and sequence IDs must fit exactly in f64".into());
        }
    }
    if options.min_rate == 0 || options.min_rate > options.max_rate {
        return Err("invalid search rate bounds".into());
    }
    if options.ring_capacity < 16 || options.ring_capacity > 1 << 30 || !options.ring_capacity.is_multiple_of(8) {
        return Err("FIT ring capacity must be an eight-byte multiple from 16 bytes to 1 GiB".into());
    }
    if let Some(side) = &options.side {
        if side != "producer" && side != "consumer" {
            return Err("profile side must be producer or consumer".into());
        }
    }
    Ok(options)
}

fn parse_transport(value: &str) -> Result<Transport> {
    match value {
        "fit" => Ok(Transport::Fit),
        "grpc" => Ok(Transport::Grpc),
        _ => Err("transport must be fit or grpc".into()),
    }
}

fn parse_workload(value: &str) -> Result<Workload> {
    match value {
        "metrics" => Ok(Workload::Metrics),
        "logs" => Ok(Workload::Logs),
        _ => Err("workload must be metrics or logs".into()),
    }
}

#[derive(Deserialize)]
struct WorkerReply {
    ok: bool,
    address: Option<String>,
    stats: Option<Snapshot>,
    error: Option<String>,
}

struct ChildLink {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    label: &'static str,
}

fn worker_command(executable: &Path, profile: Option<&Path>) -> Command {
    if let Some(profile) = profile {
        let mut command = Command::new("samply");
        command
            .args(["record", "--save-only", "-o"])
            .arg(profile)
            .arg("--")
            .arg(executable);
        command
    } else {
        Command::new(executable)
    }
}

impl ChildLink {
    fn launch(
        label: &'static str, options: &Options, endpoint: &str, profile: Option<&Path>,
    ) -> Result<(Self, Option<String>)> {
        let executable = std::env::current_exe().map_err(|error| error.to_string())?;
        let arguments = [
            "__worker".to_owned(),
            label.to_owned(),
            match options.transport {
                Transport::Fit => "fit",
                Transport::Grpc => "grpc",
            }
            .to_owned(),
            match options.workload {
                Workload::Metrics => "metrics",
                Workload::Logs => "logs",
            }
            .to_owned(),
            endpoint.to_owned(),
            options.ring_capacity.to_string(),
            options.consumer_delay_us.to_string(),
        ];
        let cpu_profile = profile.filter(|_| options.profile_kind == ProfileKind::Cpu);
        let mut command = worker_command(&executable, cpu_profile);
        if profile.is_some() && options.profile_kind == ProfileKind::Memory {
            command.env("MallocStackLogging", "1");
        }
        command
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().map_err(|error| format!("start {label}: {error}"))?;
        let input = child.stdin.take().ok_or("child stdin absent")?;
        let output = BufReader::new(child.stdout.take().ok_or("child stdout absent")?);
        let mut link = Self {
            child,
            input,
            output,
            label,
        };
        let ready = link.read_reply()?;
        Ok((link, ready.address))
    }

    fn read_reply(&mut self) -> Result<WorkerReply> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.output.read_line(&mut line).map_err(|error| error.to_string())? == 0 {
                let mut stderr = String::new();
                if let Some(error_pipe) = self.child.stderr.as_mut() {
                    let _ = error_pipe.read_to_string(&mut stderr);
                }
                return Err(format!("{} exited before replying: {}", self.label, stderr.trim()));
            }
            if let Ok(response) = serde_json::from_str::<WorkerReply>(&line) {
                if !response.ok {
                    return Err(format!("{}: {}", self.label, response.error.unwrap_or_default()));
                }
                return Ok(response);
            }
            eprintln!("{}: {}", self.label, line.trim());
        }
    }

    fn ask(&mut self, command: &serde_json::Value) -> Result<WorkerReply> {
        writeln!(self.input, "{command}").map_err(|error| error.to_string())?;
        self.input.flush().map_err(|error| error.to_string())?;
        self.read_reply()
    }

    fn snapshot(&mut self) -> Result<Snapshot> {
        self.ask(&serde_json::json!({"op":"snapshot"}))?
            .stats
            .ok_or("worker snapshot missing".into())
    }

    fn start(&mut self, phase: &Phase) -> Result<()> {
        self.ask(&serde_json::json!({"op":"start", "phase":phase}))?;
        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        let _ = writeln!(self.input, "{}", serde_json::json!({"op":"stop"}));
        let _ = self.input.flush();
        for _ in 0..500 {
            if let Some(status) = self.child.try_wait().map_err(|error| error.to_string())? {
                if status.success() {
                    return Ok(());
                }
                return Err(format!("{} exited with {status}", self.label));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(format!("{} did not exit after stop", self.label))
    }
}

impl Drop for ChildLink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Serialize)]
struct Sample {
    producer: Snapshot,
    consumer: Snapshot,
}

#[derive(Serialize)]
struct Trial {
    transport: Transport,
    workload: Workload,
    batch: usize,
    rate: Option<u64>,
    ring_capacity: usize,
    warmup_secs: u64,
    duration_secs: u64,
    consumer_address: String,
    start_ns: u64,
    end_ns: u64,
    producer: Snapshot,
    consumer: Snapshot,
    producer_setup_rss_bytes: u64,
    consumer_setup_rss_bytes: u64,
    producer_measurement_baseline_rss_bytes: u64,
    consumer_measurement_baseline_rss_bytes: u64,
    producer_average_rss_bytes: u64,
    consumer_average_rss_bytes: u64,
    producer_peak_rss_bytes: u64,
    consumer_peak_rss_bytes: u64,
    in_window_decoded: u64,
    drain_ms: f64,
    backlog_slope_per_sec: f64,
    samples: Vec<Sample>,
    pass: bool,
    failures: Vec<String>,
    instrumented_side: Option<String>,
    profile: Option<String>,
}

fn await_done(producer: &mut ChildLink, deadline_ns: u64) -> Result<Snapshot> {
    loop {
        let snapshot = producer.snapshot()?;
        if snapshot.fatal {
            return Err("producer reported a fatal transport error".into());
        }
        if snapshot.done {
            return Ok(snapshot);
        }
        if common::clock_ns() > deadline_ns {
            return Err("producer phase timed out".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn drain(consumer: &mut ChildLink, accepted: u64, deadline_ns: u64) -> Result<(Snapshot, f64)> {
    let begin = common::clock_ns();
    loop {
        let snapshot = consumer.snapshot()?;
        if snapshot.fatal {
            return Err("consumer reported a fatal transport error".into());
        }
        if snapshot.decoded + snapshot.invalid >= accepted {
            return Ok((snapshot, (common::clock_ns() - begin) as f64 / 1_000_000.0));
        }
        if common::clock_ns() > deadline_ns {
            return Err("consumer did not drain within five seconds".into());
        }
        thread::sleep(Duration::from_millis(1));
    }
}

fn trend(samples: &[Sample], start_ns: u64, end_ns: u64) -> f64 {
    let lower = start_ns + (end_ns - start_ns) / 3;
    let points: Vec<_> = samples
        .iter()
        .filter_map(|sample| {
            let timestamp = sample.consumer.timestamp_ns;
            if timestamp < lower || timestamp > end_ns {
                return None;
            }
            let backlog = sample.producer.accepted.saturating_sub(sample.consumer.decoded) as f64;
            Some(((timestamp - lower) as f64 / 1e9, backlog))
        })
        .collect();
    let count = points.len() as f64;
    if count < 3.0 {
        return 0.0;
    }
    let mean_x = points.iter().map(|point| point.0).sum::<f64>() / count;
    let mean_y = points.iter().map(|point| point.1).sum::<f64>() / count;
    let numerator = points.iter().map(|(x, y)| (x - mean_x) * (y - mean_y)).sum::<f64>();
    let denominator = points.iter().map(|(x, _)| (x - mean_x).powi(2)).sum::<f64>();
    if denominator == 0.0 {
        0.0
    } else {
        numerator / denominator
    }
}

fn classify(
    producer: Snapshot, consumer: Snapshot, in_window: u64, drain_ms: f64, slope: f64, rate: Option<u64>,
) -> Vec<String> {
    let mut failures = Vec::new();
    if producer.rejected != 0 {
        failures.push(format!("{} ring records rejected", producer.rejected));
    }
    if producer.schedule_missed != 0 {
        failures.push(format!("{} scheduled records missed", producer.schedule_missed));
    }
    if producer.accepted != consumer.decoded {
        failures.push("accepted/decoded count mismatch".into());
    }
    if producer.checksum_sum != consumer.checksum_sum || producer.checksum_xor != consumer.checksum_xor {
        failures.push("sequence checksum mismatch".into());
    }
    if consumer.invalid != 0 || consumer.duplicate != 0 || consumer.out_of_order != 0 {
        failures.push("invalid, duplicate, or out-of-order record".into());
    }
    if drain_ms > 100.0 {
        failures.push(format!("drain took {drain_ms:.1} ms"));
    }
    if let Some(rate) = rate {
        if in_window.saturating_mul(100) < producer.scheduled.saturating_mul(99) {
            failures.push("less than 99% delivered in window".into());
        }
        if slope > rate as f64 * 0.001 {
            failures.push(format!("backlog grew by {slope:.1} records/s"));
        }
    }
    failures
}

fn capture_memory_profile(worker: &ChildLink, prefix: &Path) -> Result<()> {
    let pid = worker.child.id().to_string();
    for (suffix, program, arguments) in [
        ("vmmap.txt", "vmmap", vec!["-summary", pid.as_str()]),
        ("heap.txt", "heap", vec!["-s", pid.as_str()]),
        ("allocations.txt", "malloc_history", vec![pid.as_str(), "-callTree"]),
        (
            "allocation-counts.txt",
            "malloc_history",
            vec![pid.as_str(), "-allByCount"],
        ),
    ] {
        let path = PathBuf::from(format!("{}-{suffix}", prefix.display()));
        let output = File::create(&path).map_err(|error| error.to_string())?;
        let result = Command::new(program)
            .args(arguments)
            .stdout(Stdio::from(output))
            .output()
            .map_err(|error| format!("{program}: {error}"))?;
        if !result.status.success() {
            return Err(format!(
                "{program} failed for {}: {}",
                worker.label,
                String::from_utf8_lossy(&result.stderr)
            ));
        }
    }
    Ok(())
}

fn trial(options: &Options, profile: Option<&Path>) -> Result<Trial> {
    let temporary = tempfile::Builder::new()
        .prefix("checks-ipc-")
        .tempdir_in("/tmp")
        .map_err(|error| error.to_string())?;
    fs::set_permissions(temporary.path(), fs::Permissions::from_mode(0o700)).map_err(|error| error.to_string())?;
    let fit_path = temporary.path().join("fit.sock");
    let (mut consumer, address) = ChildLink::launch(
        "consumer",
        options,
        fit_path.to_str().ok_or("invalid temporary path")?,
        profile.filter(|_| options.side.as_deref() == Some("consumer")),
    )?;
    let address = address.ok_or("consumer address missing")?;
    let (mut producer, _) = ChildLink::launch(
        "producer",
        options,
        &address,
        profile.filter(|_| options.side.as_deref() == Some("producer")),
    )?;

    let setup_producer = producer.snapshot()?;
    let setup_consumer = consumer.snapshot()?;

    let warm_start = common::clock_ns() + 500_000_000;
    let warmup = Phase {
        start_ns: warm_start,
        duration_ns: options.warmup_secs * 1_000_000_000,
        rate: options.rate,
        batch: options.batch,
        first_sequence: 0,
    };
    producer.start(&warmup)?;
    let warm_produced = await_done(&mut producer, warm_start + warmup.duration_ns + 5_000_000_000)?;
    drain(
        &mut consumer,
        warm_produced.accepted,
        common::clock_ns() + 5_000_000_000,
    )?;

    let baseline_producer = producer.snapshot()?;
    let baseline_consumer = consumer.snapshot()?;
    let start_ns = common::clock_ns() + 500_000_000;
    let end_ns = start_ns + options.duration_secs * 1_000_000_000;
    let measure = Phase {
        start_ns,
        duration_ns: options.duration_secs * 1_000_000_000,
        rate: options.rate,
        batch: options.batch,
        first_sequence: warm_produced.scheduled,
    };
    producer.start(&measure)?;
    let mut samples = Vec::new();
    let mut captured_memory = false;
    while common::clock_ns() < end_ns {
        common::sleep_until((common::clock_ns() + 100_000_000).min(end_ns));
        samples.push(Sample {
            producer: producer.snapshot()?,
            consumer: consumer.snapshot()?,
        });
        if let Some(prefix) = profile.filter(|_| options.profile_kind == ProfileKind::Memory) {
            if !captured_memory && common::clock_ns() >= start_ns + (end_ns - start_ns) / 2 {
                let selected = if options.side.as_deref() == Some("producer") {
                    &producer
                } else {
                    &consumer
                };
                capture_memory_profile(selected, prefix)?;
                captured_memory = true;
            }
        }
    }
    let window_producer = producer.snapshot()?;
    let window_consumer = consumer.snapshot()?;
    let produced = await_done(&mut producer, end_ns + 5_000_000_000)?;
    let (received, drain_ms) = drain(&mut consumer, produced.accepted, end_ns + 5_000_000_000)?;
    let mut produced = produced.delta(baseline_producer);
    let mut received = received.delta(baseline_consumer);
    produced.cpu_us = window_producer.cpu_us.saturating_sub(baseline_producer.cpu_us);
    received.cpu_us = window_consumer.cpu_us.saturating_sub(baseline_consumer.cpu_us);
    let in_window_decoded = window_consumer.decoded.saturating_sub(baseline_consumer.decoded);
    let slope = trend(&samples, start_ns, end_ns);
    let average_rss = |select: fn(&Sample) -> u64| -> u64 {
        if samples.is_empty() {
            return 0;
        }
        samples.iter().map(select).sum::<u64>() / samples.len() as u64
    };
    let producer_average_rss_bytes = average_rss(|sample| sample.producer.rss_bytes);
    let consumer_average_rss_bytes = average_rss(|sample| sample.consumer.rss_bytes);
    let failures = classify(produced, received, in_window_decoded, drain_ms, slope, options.rate);
    let profile_name = profile.map(|path| path.display().to_string());
    producer.stop()?;
    consumer.stop()?;
    Ok(Trial {
        transport: options.transport,
        workload: options.workload,
        batch: options.batch,
        rate: options.rate,
        ring_capacity: options.ring_capacity,
        warmup_secs: options.warmup_secs,
        duration_secs: options.duration_secs,
        consumer_address: address,
        start_ns,
        end_ns,
        producer: produced,
        consumer: received,
        producer_setup_rss_bytes: setup_producer.rss_bytes,
        consumer_setup_rss_bytes: setup_consumer.rss_bytes,
        producer_measurement_baseline_rss_bytes: baseline_producer.rss_bytes,
        consumer_measurement_baseline_rss_bytes: baseline_consumer.rss_bytes,
        producer_average_rss_bytes,
        consumer_average_rss_bytes,
        producer_peak_rss_bytes: produced.peak_rss_bytes,
        consumer_peak_rss_bytes: received.peak_rss_bytes,
        in_window_decoded,
        drain_ms,
        backlog_slope_per_sec: slope,
        samples,
        pass: failures.is_empty(),
        failures,
        instrumented_side: options.side.clone(),
        profile: profile_name,
    })
}

fn unique_results_dir(root: &Path) -> Result<PathBuf> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    let directory = root.join(format!("run-{stamp}"));
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    Ok(directory)
}

fn save_trial(directory: &Path, index: usize, trial: &Trial) -> Result<()> {
    let path = directory.join(format!("trial-{index:03}.json"));
    let file = File::create(path).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(file, trial).map_err(|error| error.to_string())
}

fn print_trial(trial: &Trial) {
    let secs = trial.duration_secs as f64;
    println!(
        "{:?} {:?} batch={} rate={} accepted={} received={} in_window={:.0}/s rejected={} missed={} cpu_ms={:.1} avg_rss_mib={:.1} peak_rss_mib={:.1} pass={} {}",
        trial.transport, trial.workload, trial.batch,
        trial.rate.map_or_else(|| "unlimited".into(), |rate| rate.to_string()),
        trial.producer.accepted, trial.consumer.decoded, trial.in_window_decoded as f64 / secs,
        trial.producer.rejected, trial.producer.schedule_missed,
        (trial.producer.cpu_us + trial.consumer.cpu_us) as f64 / 1000.0,
        (trial.producer_average_rss_bytes + trial.consumer_average_rss_bytes) as f64 / 1_048_576.0,
        (trial.producer_peak_rss_bytes + trial.consumer_peak_rss_bytes) as f64 / 1_048_576.0,
        trial.pass, trial.failures.join("; "),
    );
}

fn run_many(options: &Options, directory: &Path, profile: bool) -> Result<Vec<Trial>> {
    let mut trials = Vec::new();
    for index in 0..options.repetitions {
        let profile_path = profile.then(|| match options.profile_kind {
            ProfileKind::Cpu => directory.join(format!("profile-{index:03}.json")),
            ProfileKind::Memory => directory.join(format!("memory-{index:03}")),
        });
        let result = trial(options, profile_path.as_deref())?;
        save_trial(directory, index, &result)?;
        print_trial(&result);
        trials.push(result);
    }
    write_summary(directory, &trials)?;
    Ok(trials)
}

fn write_summary(directory: &Path, trials: &[Trial]) -> Result<()> {
    let mut file = File::create(directory.join("summary.csv")).map_err(|error| error.to_string())?;
    writeln!(file, "transport,workload,batch,rate,accepted,decoded,in_window_per_sec,rejected,schedule_missed,producer_cpu_ms,consumer_cpu_ms,producer_setup_rss_bytes,consumer_setup_rss_bytes,producer_average_rss_bytes,consumer_average_rss_bytes,producer_peak_rss_bytes,consumer_peak_rss_bytes,drain_ms,pass")
        .map_err(|error| error.to_string())?;
    for trial in trials {
        writeln!(
            file,
            "{:?},{:?},{},{},{},{},{:.1},{},{},{:.1},{:.1},{},{},{},{},{},{},{:.1},{}",
            trial.transport,
            trial.workload,
            trial.batch,
            trial
                .rate
                .map_or_else(|| "unlimited".to_owned(), |rate| rate.to_string()),
            trial.producer.accepted,
            trial.consumer.decoded,
            trial.in_window_decoded as f64 / trial.duration_secs as f64,
            trial.producer.rejected,
            trial.producer.schedule_missed,
            trial.producer.cpu_us as f64 / 1000.0,
            trial.consumer.cpu_us as f64 / 1000.0,
            trial.producer_setup_rss_bytes,
            trial.consumer_setup_rss_bytes,
            trial.producer_average_rss_bytes,
            trial.consumer_average_rss_bytes,
            trial.producer_peak_rss_bytes,
            trial.consumer_peak_rss_bytes,
            trial.drain_ms,
            trial.pass,
        )
        .map_err(|error| error.to_string())?;
    }
    let mut markdown = File::create(directory.join("summary.md")).map_err(|error| error.to_string())?;
    writeln!(markdown, "# Checks IPC benchmark results\n\n| Transport | Workload | Batch | Offered/s | Delivered/s | Rejected | Missed | Combined CPU ms | Average RSS MiB | Peak RSS MiB | Pass |\n| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |")
        .map_err(|error| error.to_string())?;
    for trial in trials {
        writeln!(
            markdown,
            "| {:?} | {:?} | {} | {} | {:.0} | {} | {} | {:.1} | {:.1} | {:.1} | {} |",
            trial.transport,
            trial.workload,
            trial.batch,
            trial
                .rate
                .map_or_else(|| "unlimited".to_owned(), |rate| rate.to_string()),
            trial.in_window_decoded as f64 / trial.duration_secs as f64,
            trial.producer.rejected,
            trial.producer.schedule_missed,
            (trial.producer.cpu_us + trial.consumer.cpu_us) as f64 / 1000.0,
            (trial.producer_average_rss_bytes + trial.consumer_average_rss_bytes) as f64 / 1_048_576.0,
            (trial.producer_peak_rss_bytes + trial.consumer_peak_rss_bytes) as f64 / 1_048_576.0,
            trial.pass,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn next_screen_rate(rate: u64, passing: Option<u64>, failing: Option<u64>, min: u64, max: u64) -> Option<u64> {
    match (passing, failing) {
        (Some(pass), Some(fail)) if (fail - pass) as f64 / pass as f64 <= 0.05 => None,
        (Some(pass), Some(fail)) => Some(pass + (fail - pass) / 2),
        (Some(_), None) if rate == max => None,
        (Some(_), None) => Some(rate.saturating_mul(2).min(max)),
        (None, Some(_)) if rate == min => None,
        (None, Some(_)) => Some((rate / 2).max(min)),
        (None, None) => unreachable!("screening records the current result before choosing the next rate"),
    }
}

fn search(mut options: Options, directory: &Path) -> Result<()> {
    if options.rate.is_none() {
        return Err("search requires a controlled rate".into());
    }
    options.warmup_secs = 5;
    options.duration_secs = 10;
    options.repetitions = 1;
    let mut passing = None;
    let mut failing = None;
    let mut rate = 10_000u64.clamp(options.min_rate, options.max_rate);
    let mut index = 0usize;
    let mut screening = Vec::new();
    loop {
        options.rate = Some(rate);
        let result = trial(&options, None)?;
        save_trial(directory, index, &result)?;
        print_trial(&result);
        index += 1;
        screening.push((rate, result.pass));
        if result.pass {
            passing = Some(rate);
        } else {
            failing = Some(rate);
        }
        match next_screen_rate(rate, passing, failing, options.min_rate, options.max_rate) {
            Some(next) => rate = next,
            None => break,
        }
    }
    fs::write(
        directory.join("screening.json"),
        serde_json::to_vec_pretty(&screening).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    let mut candidate = passing.ok_or("search found no passing rate")?;
    options.warmup_secs = 5;
    options.duration_secs = 30;
    options.repetitions = 5;
    loop {
        options.rate = Some(candidate);
        let candidate_dir = directory.join(format!("confirm-{candidate}"));
        fs::create_dir_all(&candidate_dir).map_err(|error| error.to_string())?;
        let results = run_many(&options, &candidate_dir, false)?;
        if results.iter().all(|result| result.pass) {
            fs::write(
                directory.join("search-summary.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "confirmed_rate": candidate, "failing_screening_bound": failing,
                    "bounded_below_by_ceiling": candidate == options.max_rate,
                }))
                .map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            println!(
                "CONFIRMED {} records/s; failing screening bound {:?}",
                candidate, failing
            );
            return Ok(());
        }
        let lower = candidate * 9 / 10;
        if lower < options.min_rate || lower == candidate {
            return Err("no confirmed rate within bounds".into());
        }
        candidate = lower;
    }
}

fn provenance(directory: &Path) -> Result<()> {
    let command_output = |program: &str, arguments: &[&str]| -> String {
        Command::new(program)
            .args(arguments)
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let profile = if executable
        .components()
        .any(|part| part.as_os_str() == "optimized-release")
    {
        "optimized-release"
    } else {
        "other (not suitable for headline comparisons)"
    };
    let metadata = serde_json::json!({
        "git_head": command_output("git", &["rev-parse", "HEAD"]),
        "git_status": command_output("git", &["status", "--short"]),
        "rustc": command_output("rustc", &["--version"]),
        "os": command_output("uname", &["-a"]),
        "hardware": command_output("sysctl", &["-n", "machdep.cpu.brand_string"]),
        "executable": executable,
        "executable_sha256": command_output("shasum", &["-a", "256", executable.to_str().unwrap_or("")]),
        "profile": profile,
        "catch_up_ns": common::CATCH_UP_NS,
    });
    fs::write(
        directory.join("provenance.json"),
        serde_json::to_vec_pretty(&metadata).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    fs::copy(&executable, directory.join("checks-ipc-bench"))
        .map_err(|error| format!("preserve benchmark executable: {error}"))?;
    Ok(())
}

fn usage() -> &'static str {
    "checks-ipc-bench run|search|profile [--transport fit|grpc] [--workload metrics|logs] [--batch N] [--rate N|unlimited] [--ring BYTES] [--warmup SECONDS] [--duration SECONDS] [--repetitions N] [--output DIR] [--side producer|consumer] [--kind cpu|memory] [--min-rate N] [--max-rate N] [--consumer-delay-us N]"
}

fn main() {
    if let Err(error) = real_main() {
        eprintln!("{error}\n{}", usage());
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let Some(operation) = args.first().map(String::as_str) else {
        return Err("missing command".into());
    };
    if operation == "__worker" {
        if args.len() != 7 {
            return Err("invalid worker arguments".into());
        }
        return worker::run(
            &args[1],
            parse_transport(&args[2])?,
            parse_workload(&args[3])?,
            args[4].clone(),
            args[5].parse().map_err(|_| "invalid worker ring capacity")?,
            args[6].parse().map_err(|_| "invalid worker consumer delay")?,
        );
    }
    let options = parse_options(&args[1..])?;
    let directory = unique_results_dir(&options.output)?;
    provenance(&directory)?;
    match operation {
        "run" => {
            run_many(&options, &directory, false)?;
        }
        "search" => search(options, &directory)?,
        "profile" => {
            if options.side.is_none() {
                return Err("profile requires --side producer|consumer".into());
            }
            if options.profile_kind == ProfileKind::Cpu {
                if !Command::new("samply")
                    .arg("--version")
                    .output()
                    .is_ok_and(|output| output.status.success())
                {
                    return Err("Samply is unavailable; install it with `cargo install --locked samply`".into());
                }
            } else if !cfg!(target_os = "macos") {
                return Err("memory profiling currently uses macOS heap tools".into());
            }
            let symbol_status = Command::new("dsymutil")
                .arg(directory.join("checks-ipc-bench"))
                .arg("-o")
                .arg(directory.join("checks-ipc-bench.dSYM"))
                .status()
                .map_err(|error| format!("generate macOS debug symbols: {error}"))?;
            if !symbol_status.success() {
                return Err(format!("dsymutil failed with {symbol_status}"));
            }
            run_many(&options, &directory, true)?;
        }
        _ => return Err(format!("unknown command: {operation}")),
    }
    println!("Raw results: {}", directory.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loss_and_pacing_fail_capacity() {
        let producer = Snapshot {
            scheduled: 100,
            accepted: 99,
            rejected: 1,
            ..Snapshot::default()
        };
        let consumer = Snapshot {
            decoded: 99,
            ..Snapshot::default()
        };
        assert!(classify(producer, consumer, 99, 1.0, 0.0, Some(100))
            .iter()
            .any(|failure| failure.contains("rejected")));
        let producer = Snapshot {
            scheduled: 100,
            attempted: 99,
            accepted: 99,
            schedule_missed: 1,
            ..Snapshot::default()
        };
        assert!(classify(producer, consumer, 99, 1.0, 0.0, Some(100))
            .iter()
            .any(|failure| failure.contains("missed")));
    }

    #[test]
    fn search_input_and_backlog_slope() {
        assert!(parse_options(&["--rate".into(), "unlimited".into()])
            .unwrap()
            .rate
            .is_none());
        assert!(parse_options(&["--batch".into(), "0".into()]).is_err());
        assert_eq!(trend(&[], 0, 1), 0.0);
        assert_eq!(next_screen_rate(10_000, Some(10_000), None, 100, 100_000), Some(20_000));
        assert_eq!(
            next_screen_rate(20_000, Some(10_000), Some(20_000), 100, 100_000),
            Some(15_000)
        );
        assert_eq!(next_screen_rate(10_400, Some(10_000), Some(10_400), 100, 100_000), None);
        assert_eq!(next_screen_rate(100, None, Some(100), 100, 100_000), None);
        assert_eq!(next_screen_rate(100_000, Some(100_000), None, 100, 100_000), None);
    }

    #[test]
    fn profile_command_keeps_worker_arguments_after_separator() {
        let mut command = worker_command(Path::new("/tmp/fake-bench"), Some(Path::new("/tmp/cpu.json")));
        command.args(["__worker", "producer", "fit"]);
        assert_eq!(command.get_program(), "samply");
        let args: Vec<_> = command
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "record",
                "--save-only",
                "-o",
                "/tmp/cpu.json",
                "--",
                "/tmp/fake-bench",
                "__worker",
                "producer",
                "fit"
            ]
        );
    }
}
