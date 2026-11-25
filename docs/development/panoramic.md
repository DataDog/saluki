# Panoramic cases

Panoramic is the test runner for the integration and correctness suites. This page is for people who write or run
cases. [Testing](./testing.md) says what each kind of test is for.

A case is a directory with a `config.yaml` and the files it uses. Panoramic runs the participants the case names,
runs the steps of its procedure against them, and reports what happened. The same file runs on each OS it declares.

## The two suites

The cases live in two directories, one for each purpose. Panoramic runs both the same way, and any step can appear
in either. The directory says what a case is for.

- **`test/integration/cases`** checks ADP on its own. A case starts ADP, usually with the Agent, and checks that it
  starts, stays up, logs what it should, listens on its endpoints, exits as expected, and sends the expected metrics to
  a mock intake. Each case runs on every OS in its `subject_os`: Linux, macOS, Windows, or more than one. See
  [Writing an integration case](#writing-an-integration-case).
- **`test/correctness/cases`** compares ADP with the Datadog Agent. A case sends the same load to both, gives each
  its own mock intake, and ends with a `compare` step. The cases run on Linux. See
  [Writing a comparison case](#writing-a-comparison-case).

## What a case is

- **The name.** The directory names the case. The name must be unique among the cases of one run. Panoramic loads
  only the direct subdirectories of a `-d` directory that hold a `config.yaml`.
- **The suite.** The directory above `cases/` names the suite. `test/integration/cases/adp-cmd-host` is in the
  `integration` suite, and `test/correctness/cases/dsd-plain` is in `correctness`. The suite names the case's log
  directory.
- **The subject.** One participant sets `subject: true`. It is the software under test.
- **Participants.** A participant is one piece of software: an image, a pod, or programs on the host. A case names
  each one. A participant name is a lowercase DNS label, because it becomes a container name and a host name.
  `self`, `localhost`, names of only digits and `0x` numbers are errors. The names of endpoints, processes, commands
  and connections start with a lowercase letter and hold lowercase letters, digits and `_`.
- **Participant types.** `type:` names a piece of software that panoramic knows: `agent-adp`, `agent`,
  `datadog-intake` or `millstone`. The type fills in the image, the command, the environment, the endpoints and
  the checks of the software. A participant with no type writes all of that itself.
- **Realization.** A realization is how a participant runs: a Linux container, a Windows container, a pod, or host
  processes. The subject runs in the default of the subject OS: a Linux container on Linux, host processes on macOS,
  and a Windows container on Windows. Every other participant follows the subject. A case sets `realization` only
  when the realization is under test.
- **`subject_os`.** The list of OSes the case runs on: `linux`, `macos` or `windows`. A run selects one. A case that
  does not list it is skipped, with the reason.

The working examples are in `bin/correctness/panoramic/testdata/examples/`. The cases under
`bin/correctness/panoramic/testdata/bad/` show the errors that panoramic finds before it starts anything. Each one
has the substring of its error in `expected.yaml`.

## Running cases

Use the Make targets. They build what a run needs.

```bash
make test-integration          # build the images, run every integration case
make test-integration-quick    # run every integration case, with the images that exist
make list-integration-tests    # list the integration cases
make test-correctness          # run every correctness case
make test-correctness-case CASE=dsd-plain
make test-integration-macos-ci # on macOS: run the cases that declare macOS
```

`panoramic` takes the same options when you run it yourself. Build it with `make build-panoramic` and the courier
with `make build-courier`. The courier is the program panoramic runs inside containers and pods.

```bash
target/release/panoramic run -d test/integration/cases -t adp-cmd-host --no-tui
target/release/panoramic list -d test/integration/cases --json --subject-os linux
target/release/panoramic plan test/integration/cases/adp-cmd-host --subject-os linux
```

| Option | Meaning |
| --- | --- |
| `-d <dir>` | A directory of cases. Repeat it for more than one. |
| `-t <names>` | Run only these cases, as a comma-separated list. A name that matches nothing is an error. |
| `--subject-os <os>` | The OS to run for. It defaults to the OS of the host. |
| `--set <name>=<reference>` | Run `<reference>` where a case names the image or host program `<name>`. |
| `-p <n>` | How many cases run at once. The default is 4. |
| `-f`, `--fail-fast` | Stop after the first failure. |
| `--no-tui` | Print plain text, not the terminal display. |
| `-o json` | Print the run report as JSON. |
| `-l <dir>`, `PANORAMIC_LOG_DIR` | The base directory for logs. The default is the temporary directory. Each run makes a timestamped subdirectory. |
| `-v` | Print the details of every step. |
| `--log-level <level>` | The log level of panoramic and its libraries. The default is `info`. |
| `--kind-cluster-name <name>`, `--no-delete-kind-cluster` | The kind cluster of the cases that run pods, and whether to keep it after the run. |

`PANORAMIC_SET` takes the `--set` entries as a comma-separated list. CI uses it to run the cases against the images a
pipeline built. `NAME` is the reference exactly as the case or the participant type writes it. A name given twice is
an error. A name that no case uses is an error too.

On macOS, `--set` also runs the host-process cases against other builds or installs of ADP and the Agent. This example
uses a debug build of ADP and the system-wide Agent in `/opt/datadog-agent`:

```bash
target/release/panoramic run -d test/integration/cases --no-tui \
  --set target/release/agent-data-plane=target/debug/agent-data-plane \
  --set /tmp/saluki-dda/datadog-agent/bin/agent/agent=/opt/datadog-agent/bin/agent/agent
```

`make test-integration-macos-run` sets the two names itself. To give it another Agent, set
`MACOS_TEST_AGENT_INSTALL_DIR`.

`panoramic list` leaves out a case that does not declare the subject OS. With `--json` it prints one entry for each
case with its `suite`, its `runtime` and its `timeout`. The `runtime` is `linux`, `mac` or `windows`, or
`kubernetes_in_docker` for a case that runs pods. The CI generator reads it.

`panoramic plan <case> --subject-os <os>` prints what the case does on that OS as JSON, and starts nothing. `--text`
prints the same for people. It shows each participant's realization, image, command, environment and files, each
endpoint and connection with its address, and the order in which participants start. It plans the case for every OS in
`subject_os`, so a field that fails on any of them is an error on any host.

### Checks before the first case

Before any case starts, one check covers the whole selection. It runs outside the timeout of every case.

1. Each realization that the cases need is available: a Docker daemon for containers, `kind` for pods, and the
   right host OS for host processes. A Windows container needs a Windows host. Host processes need macOS or Linux.
2. Each image is in the daemon. Panoramic pulls an image that is missing, once.
3. Each host program exists and runs. `./x` is a file of the participant, so it is found later. A bare name is
   found on the `PATH`. Any other path is relative to the directory panoramic runs in.
4. The courier exists for each container OS in use.
5. For pods, the kind cluster exists and holds the images. Panoramic makes a missing cluster. The cluster must
   have one node.

A failed check is a harness error that names the case. The run stops with exit code 2 before any case starts.

### Which cases run at once

Cases in containers and pods run in parallel, up to `-p`. Cases with host processes run one at a time, because their
ports are fixed.

### CI

| Job | What it runs |
| --- | --- |
| `test-integration` | Every integration case on Linux, with the images the pipeline built. |
| `test-integration-macos-arm64`, `test-integration-macos-amd64` | The cases that declare macOS, as host processes. |
| `test-integration-windows-amd64` | The cases that declare Windows, in Windows containers. |
| `generate-correctness-pipeline` | Makes one `test-correctness-<case>` job for each correctness case. |
| `run-correctness-tests` | Runs the generated jobs. A case with pods gets the longer kind template. |

## The case file

```yaml
description: <text>                    # optional
timeout: <duration>                    # required, with a unit: 20m, 180s
disabled: true                         # optional: the case is checked, and no run includes it
subject_os: [linux, macos, windows]    # required, at least one
participants:
  <name>: <participant>                # exactly one has subject: true
procedure:
  - <step>
  - parallel: [<step>, ...]            # assertions only, no nesting
```

A key that its owner does not know is an error before anything starts. The cases under `testdata/bad/` pin such
errors.

### A participant

| Field | Meaning |
| --- | --- |
| `type` | A participant type. Without it, the case writes the participant in full. |
| `subject` | `true` for the software under test. |
| `realization` | `{enclosure, options}`. The enclosures are `linux-container`, `windows-container`, `kubernetes-pod` and `host-process-group`. |
| `connect` | `{<name>: <participant>.<endpoint>}`: the endpoints this participant uses. |
| `with` | Parameters of the participant type. It is an error without `type`. |
| `image`, `command`, `args` | What runs in a container or pod. `command` replaces the entrypoint of the image, and `args` replaces its arguments. |
| `programs` | A list of `{name, path, args, env}`. What runs as host processes. |
| `env` | `{NAME: "text"}`. The environment of every process. Quote each value. |
| `files` | `{<destination or name>: <source>}`. Files placed before the participant starts. |
| `endpoints` | `{<name>: {protocol, port \| path \| name}}`. Where the participant listens. |
| `ready` | `{<endpoint>: <evidence>}`. How panoramic knows an endpoint is ready. |
| `processes` | A list of `{name, exit, output}`: the processes that steps name, and how each is observed. See [Observation](#observation). |
| `commands` | `{<name>: [<program>, <arg>, ...]}`, or `{program, argv, env}`. Command lines that `exec` and the operations run by name. `program` names a program of host processes. |
| `mounts` | A list of `{host, path, read_only}`. Host paths in a Linux container or pod. |
| `collect` | A list of `{path, name, keep}`. Files copied out after the procedure. `keep` is `always` or `on_failure`. |

The case's fields lie over the fields of the participant type. A map merges by key, and an entry of the case replaces
the entry of the type with the same key. Every other field replaces the field below it. A case that sets `command`
and not `args` drops the `args` of the type. The type's own checks run on the result.

A case that sets `image` on host processes, or `programs` in a container, is an error. Two more guards stop a change
that would fail without a message:

- A case that sets `command` or `args`, or replaces a file that a participant type uses to supervise its processes,
  must set `processes` too. Without it, no exit record is written, and a process that never started reads as running.
- A case that makes a connection must not set the environment variable that the connection delivers. The case's value
  would win over the delivered one.

`realization.options` are booleans. A Linux container takes `host_pid` and `host_cgroup_namespace`. A pod takes
`host_pid` and `cri_socket`.

### Files

A key that looks like a path is a destination. Any other key must be a name that the participant type declares, such
as `datadog.yaml`. A name belongs to some realizations only. `agent-adp` and `agent` declare `datadog.yaml`, and `agent`
declares `otel-config.yaml`, in a Linux container or pod. On macOS and Windows, write a destination: `./datadog.yaml`
for macOS host processes, and `C:/ProgramData/Datadog/datadog.yaml` for a Windows container. Planning checks every OS
in `subject_os`, so a name that one of them does not declare is an error even when the case runs on another.

| Realization | A destination is |
| --- | --- |
| Linux container, pod | An absolute path. |
| Windows container | A path on drive `C:`, written with `/` or `\`. |
| Host process | `./…`, under the directory of the participant, with no `..`. |

A destination that ends in a separator is a directory. The value is a source path, relative to the case directory, or
`{from, template, mode}`. A source stays inside the suite directory. `template: true` marks a UTF-8 file whose
references panoramic fills in. Every other file is copied byte for byte. `mode` is a quoted octal string such as
`"0755"`, and the default is `"0644"`. `mode` is an error on a Windows participant. Two entries that land on one path
are an error, and so is a path that panoramic reserves.

### References

::: v-pre

A reference is a value that panoramic owns, written between double braces. It has one of these forms:

- `{{ self.endpoints.api.port }}`: the `port`, `path` or `name` of an endpoint. `self` is the participant that holds
  the text. In a step, a participant name replaces `self` to name another participant's endpoint. A participant cannot
  do that: it declares `connect` and reads the endpoint through `self.connections`.
- `{{ self.connections.intake.url }}`: the `url`, `host`, `port` or `path` of an endpoint that this participant
  connects to.
- `{{ self.dir }}`: the directory of the participant. `ip`, `container_id` and `pod_uid` read the same way.

A reference is valid in `command`, `args`, `env`, the programs' arguments and environment, a template file, a
readiness pattern and the arguments of a named command. A step takes one only in its template fields. Each `{{`
starts a reference, and there is no escape. Panoramic checks that every reference names something that exists and
that its value is known in time.

### Endpoints and connections

An endpoint has a protocol and one address field.

| Protocol | Address |
| --- | --- |
| `tcp`, `udp`, `http`, `grpc` | `port`. `http` takes `tls: true`. |
| `unix_datagram`, `unix_stream` | `path` |
| `named_pipe` | `name` (Windows only) |

The harness owns each address. A case does not pick a free port: it writes the reference, and panoramic fills in the
value. A participant type already wires its own listeners through its default environment.

`connect: {name: other.endpoint}` makes the endpoint of another participant known to this one. A participant type
names the connections it understands, and each one sets environment for the software. `agent-adp` and `agent` take
`intake` and `dogstatsd_forward`. `millstone` takes `comparison_target` and `baseline_target`. A participant with no
type reads its connections through `{{ self.connections.<name>.url }}`. A connection that nothing uses is an error.

`ready` says when an endpoint is ready: `accepts`, `exists`, `none`, `{http: <path>}` or
`{output: <pattern>, process: <name>}`. A participant that listens on an endpoint starts before the participants that
connect to it.

:::

### Errors that panoramic finds first

`list`, `plan` and `run` load every case and plan it for every OS in `subject_os`. The errors include:

- an unknown key, more or less than one subject, or a participant type that does not support the realization;
- a file source that is missing, or outside the suite directory;
- two files on one path, or a destination that does not fit the realization;
- two endpoints with one address, or a `connect` target that does not exist;
- a reference to something that does not exist, or whose value is not known in time;
- a cycle in the order of creation or start;
- a step that does not decode, or that names a participant, process, command, operation or endpoint that does not
  exist on that OS.

Panoramic cannot check that a program exists in an image, that a script is correct, or that the software opens its
listeners. Those fail at start, in `exec`, or at a readiness timeout, with the output attached.

## Participant types

| Type | Software | Realizations |
| --- | --- | --- |
| `agent-adp` | The Datadog Agent with ADP. | Linux container, pod, macOS host processes, Windows container |
| `agent` | The Datadog Agent alone, as the baseline of a comparison. | Linux container, pod |
| `datadog-intake` | The mock intake. It records what it receives. | Linux container, pod |
| `millstone` | The load generator. | Linux container, pod |

**`agent-adp` and `agent`.** The default image is `saluki-images/datadog-agent:testing-devel`, and a case writes
another image with `image:`. Both take the default environment from `DEFAULT_ENV` in
`bin/correctness/panoramic/src/participants/datadog_agent.rs`. That environment moves every listener off its default
port. A port that a case binds must be below 49152, because
macOS hands out the ports above it to outbound connections.

A case that sets a key of `datadog.yaml` hides the default environment entry of the same setting. The default entry
is withdrawn, and the case must wire the endpoint itself. Panoramic says so when it does not. The same holds when the
case sets `ADP_DD_<NAME>` for `DD_<NAME>`, or another name that the software reads for the setting.

- *Endpoints of `agent-adp`.* `dogstatsd_udp`, `apm_tcp`, `api`, `agent_otlp_grpc` and `agent_otlp_http` exist by
  default. Opt-in endpoints are `otlp_grpc`, `otlp_http`, `dogstatsd_tcp`, `dogstatsd_uds`, `dogstatsd_uds_stream`
  and `apm_uds` (Linux only), and `dogstatsd_pipe` on Windows. The case turns one on when it writes the reference
  under the key that the software reads.
- *Endpoints of `agent`.* `dogstatsd_udp`, `apm_tcp`, `agent_otlp_grpc`, `agent_otlp_http` and `otel_otlp_grpc`
  exist by default. `otel_otlp_grpc` is for the collector in `otel-config.yaml`, which the case places as a template.
  `dogstatsd_uds`, `dogstatsd_uds_stream` and `apm_uds` are opt in.
- *`with`.* `host_view: true` mounts `/proc` and the cgroup tree of the host, and the Docker socket where there is
  one, and points the software at them. It is not for host processes or Windows. `own_ip: {env: [...], hosts: [...]}`
  sets each variable in `env` to the address of the participant, and maps each name in `hosts` to it. It is for
  `agent-adp` in a container.
- *Files.* In a Linux container or pod, `datadog.yaml` is a name that stands for the file the software reads. `agent`
  also has `otel-config.yaml`. Other realizations take a destination. See [Files](#files).
- *Processes.* `agent-adp` has `adp` and `agent`. `agent` has `agent`.
- *Commands.* `agent-adp` names `adp` and `agent`.
- *Operations.* `agent-adp` offers `config_dump`, `config_set`, `dogstatsd_capture`, `dogstatsd_replay` and
  `dogstatsd_stats`.
- *macOS.* ADP and the Agent are two programs. The paths are `target/release/agent-data-plane` and
  `/tmp/saluki-dda/datadog-agent/bin/agent/agent`. `--set` replaces each. `host_view` and `ADP_DD_` keys are errors.
- *Windows.* The image is `saluki-images/agent-data-plane:testing-windows`. It adds no entrypoint of its own.
  Panoramic replaces the base image's entrypoint with one that starts the Agent and then ADP, writes their logs and
  exit records, and restarts nothing. It keeps the container up after both exit, so panoramic can read what they left.

**`datadog-intake`.** It takes no `with`. It has the endpoints `api` (HTTP) and `dogstatsd_forward` (UDP), and the
operation `dump`. It runs from `saluki-images/correctness-tools:latest`. Connect an `agent-adp` or `agent` participant
to its `api` endpoint as `intake`.

**`millstone`.** It runs two senders in one container with the same seed, so each sends the same payloads. The case
supplies `millstone.yaml` with `template: true`, and its target is a field of the `comparison_target` connection.
Panoramic derives the file of the baseline sender from it. The processes are `comparison` and `baseline`. In a
container, the participant also collects the input capture of the baseline sender.

## Steps

A step is a map with a `step` key and the fields of the step. Every field is required unless the table says so.
`on` names a participant. A duration has a unit: `500ms`, `30s`, `5m`. A step has a budget, which is its `timeout`,
`duration` or `during`. A step that does not report within its budget and 10 seconds of slack is `errored`. The case
timeout stops a step that is still running, and the step is `interrupted`. A step takes a reference only in its
template fields: `pattern` of the two log steps, `path` of `file_contains`, and `command`, `args` and `env` of `exec`.

| Step | Fields | What it checks or does |
| --- | --- | --- |
| `log_contains` | `on`, `process` (optional), `pattern`, `regex` (optional), `timeout` | A line of the process output holds the text. Without `process`, it searches every process and the container log. |
| `log_not_contains` | `on`, `pattern`, `regex` (optional), `during` | Fails as soon as a line holds the text, whether the line is older than the step or arrives during `during`. Passes after `during` with no match. It searches every process and the container log. |
| `process_stable_for` | `on`, `process` (optional), `duration` | The process, or every process, keeps running. |
| `exits_with` | `on`, `process`, `code`, `timeout` | The process exits with the code. A death by signal has code 256. |
| `port_listening` | `on`, `endpoint`, `timeout` | The endpoint accepts a connection. |
| `http_check` | `on`, `endpoint`, `path`, `status`, `body` (optional), `insecure_skip_verify` (optional), `timeout` | A `GET` answers as expected. `status` is `{equal: n}` or `{not_equal: n}`, and `body` is `{equal: text}` or `{contains: text}`. It sends no client certificate, so it cannot reach the privileged routes of ADP. |
| `file_contains` | `on`, `path`, `pattern`, `timeout` | A file in the participant holds the text. |
| `config_key_equals` | `on`, `key`, `value`, `view`, `timeout` | A key of the ADP configuration has the value. `view` is `source` or `runtime`. |
| `intake_has_metric` | `on`, `name`, `metric_type`, `value`, `tags`, `tags_match` (all optional), `timeout` | An intake holds a metric that meets every criterion. |
| `compare` | `telemetry`, `comparison`, `baseline`, and options | The intake participants `comparison` and `baseline` hold the same telemetry. `telemetry` is `metrics`, `traces`, `logs`, `events` or `service_checks`. The options are `require_dogstatsd_forwarded_packets`, `otlp_direct_analysis_mode`, `additional_span_ignore_fields` and `traces` (traces only), and `logs` (logs only). It has no budget of its own: it has the rest of the case timeout. |
| `exec` | `on`, `command`, `args` (optional), `env` (optional), `output_contains` (optional), `timeout` | Action. Runs a command in a running participant. `command` is a list, or the name of a command of the participant. |
| `send` | `on`, `endpoint`, `payload`, `repeat` (optional, default 1), `timeout` | Action. Writes the text of `payload` to an endpoint `repeat` times, one datagram or connection each. |
| `config_set` | `on`, `key`, `value`, `timeout` | Action. Sets a key of the runtime configuration, with the text `value`. It retries until `timeout`. |
| `dogstatsd_replay` | `on`, `endpoint`, `payloads`, `payload_interval` (optional), `capture_duration`, `stats_duration`, `expected_metrics`, `timeout` | Action. Sends payloads while a capture runs, then replays the capture until the statistics hold every expected metric. `stats_duration` is a whole number of seconds. |

An `exec` that fails quotes the output of the command. A step needs the participants, processes, endpoints, commands
or operations that it names, and the plan checks each for every OS. Three steps need an operation of `agent-adp`:
`config_key_equals`, `config_set` and `dogstatsd_replay`. `intake_has_metric` and `compare` need the `dump` operation
of `datadog-intake`.

**`intake_has_metric`.** Each poll normalizes the metrics in the intake per context and type: a context is the
name and the tags, with the host as a `host:<value>` tag. Counts and rates are summed, sketches are merged, and a
gauge takes its latest value, so a value split across flushes is compared as its total. Expect totals: a partial sum can match an
early poll. Contexts are never combined, and one context must meet every criterion. `value` uses approximate float
equality, and is an error with `metric_type: sketch`. `tags_match` is `subset`, the default, or `exact`. Both sides
of the tags are sorted and deduplicated first. Under `exact`, `tags: []` means no tags and no host. `exact` does not
show that no other context arrived. A failure lists the contexts with the expected name, or the other names when
none has it.

**Known differences of traces.** A traces `compare` reports every difference in the APM statistics, not only the
first. `traces: {known_differences: [...]}` lists the differences that an open gap explains, as
`logs: {known_differences: [...]}` does for logs. An entry has `gap`, `issue` (the number of the GitHub issue),
`paths`, `kinds` (optional), `when` (optional) and `reason`. With `otlp_direct_analysis_mode`, the stats are not
compared, so the step takes no entries. A path is `stats.groups`, for a
statistics group on one side only, or `stats.<field>`, for a field of a group on both sides. `stats.<field>`
covers a difference anywhere inside the field, such as in a sketch. A kind is `missing`
(only in the baseline), `extra` (only in the comparison) or `value`. `when` matches the fields of the group, for
example `{service: [api-gateway]}`. A difference that no entry covers fails the step, and so does an entry path that
matches no difference, so that a fix removes its entries.

Steps run in order. A step that does not pass stops the procedure, and the steps after it are `not_run`. A
`parallel` block runs its members together, and holds assertions only. A step reaches a participant through its
operations and endpoints, so it does not depend on the participant type.

## Observation

Every process of a participant has one exit channel and one output channel.

| Process | Exit | Output |
| --- | --- | --- |
| A host process | The wait result of the child | Its standard output and error |
| The only process of a container or pod | The exit of the container | The container log |
| A supervised process | A record file that a supervisor appends to | A log file that panoramic follows |

An exit has three states: running, exited with a status, or undetermined. Undetermined is an observation error. A
step that needs the status errors, and no code is invented.

A supervisor in a container or pod writes an exit record. Panoramic gives every container and pod two environment
variables, `PANORAMIC_EXIT_DIR` and `PANORAMIC_LOG_DIR`. The format of the record is a contract:

- The record is the file `$PANORAMIC_EXIT_DIR/<name>`, where `<name>` is the `record` of the process.
- The supervisor appends one JSON object for each exit, on its own line: `{"code": 0, "signal": 0, "time":
  "1700000000.25"}`.
- `time` is a Unix time in seconds, with one to nine fractional digits.
- A normal exit has `signal` 0. A death by signal has `code` 256 and the number of the signal.
- A record with code 256 and no signal, or a signal and another code, is an observation error.
- Other fields are ignored, so the format can grow.
- A line counts once its newline is written.
- The first line is final. A restart never returns a process to running.

A container reports a death by signal n as code 128 + n. A record and a host program report 256 and the signal. No
case asserts on a death by signal.

A process writes its output under `<log dir>/<participant>/<process>.log`. A participant with only supervised
processes also has its container log, `_container.log`. A step that names a process searches that process. A
`log_contains` with no process also searches the container log.

A case declares a supervised process in `processes`: `{name, exit: {record: <file>}, output: {file: <file>}}`. Each of
`exit` and `output` can also be `container`, for what the realization reports. A `<file>` is a plain file name. The
case must place the supervisor that writes the files.

## Host processes

A participant with `host-process-group` runs its programs on the machine that runs panoramic.

- The state directory is `/tmp/panoramic`, for every case. Panoramic refuses to start while it exists. The macOS
  job removes it first.
- One case with host processes runs at a time. The ports are fixed, so two such cases would collide.
- A port that a case binds is below 49152. macOS hands out the ports above it as source ports, and refuses to bind
  a port that another process holds.
- A program runs with a cleared environment, plus `PATH`, `HOME` and `TMPDIR` of the runner.
- Panoramic stops a program with SIGTERM to its process group, and SIGKILL after two seconds. A descendant that
  leaves the group, for example with `setsid`, does not get the signal, and it may outlive the case.
- A process has no record and no output file. A host process on Windows is not built.
- A program that dies by signal n has code 256 and the signal.
- Requests go from the runner to the port of the endpoint, so TLS requests work.

## Results

Each run makes a directory `panoramic-<timestamp>` under the log base. The Make targets use
`target/test-output/panoramic/`. Set `SALUKI_TEST_OUTPUT_DIR` to move the shared root, or `PANORAMIC_LOG_DIR` to move
the base for panoramic alone. `make clean-test-logs` deletes it.

| File | Holds |
| --- | --- |
| `run.json` | The whole run: the state, the totals, the selected cases and their reports. |
| `<suite>/<case>/result.json` | One case: its outcome, `assertions` (one entry for each step), `phase_timings`, participants, `reclaim` entries and issues. |
| `<suite>/<case>/result.log` | A short text summary of the case. |
| `<suite>/<case>/plan.json` | The plan of a case that did not pass, as `panoramic plan` prints it. |
| `<suite>/<case>/<participant>/` | The logs of each process, and `collected/` with the files the case collects. |

`run.json` changes during the run, so a passing entry does not show that the run finished. Read the state first. Its
`runtime` key names the subject OS as `linux`, `mac` or `windows`.

```bash
jq '{state, interrupted_reason, running, not_run}' run.json
jq -r '.tests[] | select(.outcome != "passed" or (.issues | length) > 0) | "\(.name) \(.outcome) \(.log_dir)"' run.json
```

Treat an unknown state, outcome or status as a failure. Check `issues` on a pass: they do not change the outcome. The
report types are in `bin/correctness/panoramic/src/machine_output.rs`, and `schema_version` tells the version. The
runner writes `plan.json` from the settled outcome, so a case that does not pass keeps it, even a case that the run
cancelled or dropped after its cleanup grace. A skipped case has no plan. The report lists the file under
`artifacts`.

The exit code of a run is:

| Code | Meaning |
| --- | --- |
| 0 | Every case passed. |
| 1 | A step failed. |
| 2 | A harness error, a case that timed out, or an interrupted run. |
| 3 | Nothing ran: an empty selection, a name that matches nothing, or every case skipped. |

An `errored` step could not be evaluated. Read the `error`, `timeout`, `cause` and step `message` fields first.

**Collected files.** A file that a participant lists in `collect`, or that a step writes, has `keep: always` or
`keep: on_failure`. Panoramic applies the rule to the outcome, and `result.json` lists the files it kept.

**Cleanup.** After every case, panoramic removes what the case made and records each resource as `reclaimed`,
`leaked` or `unsettled`, under `reclaim` in `result.json`. A container case has one `case_scope` entry for its
containers, network and volume. Host processes have `host_process` entries, and the state directory has a `directory`
entry. A pod case has a `namespace` entry and a `node_directory` entry for its directory on the kind node. A failed
removal adds a `resource_leaked` issue, and an unconfirmed one adds `resource_unsettled`. Neither changes the
outcome.

Send SIGINT or SIGTERM to stop a run with cleanup. A second signal can leave containers behind.

## Writing an integration case

An integration case runs `agent-adp` as the subject, and a `datadog-intake` when it checks what ADP sends. Copy a
case from `test/integration/cases` that uses the same steps.

- When ADP must stay up, end with a `parallel` block of `process_stable_for` and a `log_not_contains` on
  `panicked at|panic: |PANIC` with `regex: true`, so a crash fails the case. When ADP must exit, use `exits_with`.
- Give each step a budget that covers the start of the Agent and ADP. The cases give a readiness step 30 to 60
  seconds, and an `intake_has_metric` 120 seconds to cover a flush.
- Before a `send` to a UDP endpoint, wait for the listener's own startup log with `log_contains`. A probe cannot
  confirm a UDP listener, so `port_listening` is not a readiness gate for it.
- Use `config_key_equals`, `config_set` or an `exec` of the `adp` command for the privileged API of ADP.
  `http_check` cannot reach it.
- List only the OSes in `subject_os` where the case passes. On macOS and Windows, write a destination for each file.
  See [Files](#files).

## Writing a comparison case

A comparison case runs the Agent and ADP on the same load and compares what each sends to its own intake. Copy
`test/correctness/cases/dsd-plain`.

- One `agent-adp` participant is the subject, and one `agent` participant is the baseline. Both name the image
  with `image:`. A run replaces it with `--set`, and one entry replaces both.
- Each one has its own `datadog-intake`, and connects to it as `intake`.
- `millstone` connects `comparison_target` to the endpoint of ADP, and `baseline_target` to the endpoint of the
  Agent. Both senders send the same payloads.
- Wait for the load to finish first. `log_contains` on `load`, with `process: comparison` and then `process: baseline`,
  waits for `Millstone finished sending`.
- Wait for both sides to flush. A sender stays alive once it has sent everything, so the Agent and ADP can still
  resolve its identity. A `process_stable_for` on `load` for a little longer than an aggregation bucket gives them
  the time to flush.
- End with `compare`, and name the intake of ADP as `comparison` and the intake of the Agent as `baseline`.

The correctness cases have 8 or 12 minutes to run. A case with pods runs on kind: panoramic creates a missing
cluster, with one node.
