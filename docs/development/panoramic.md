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
| `--kind-setup-timeout <timeout>` | How long making the kind cluster and loading the images may take before the first case, written like a case `timeout`. The default is `30m`. |

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

A case's `procedure` tells panoramic what to do after the participants start. Each `step` names either an **action**
that changes something or an **assertion** that checks a condition. The other fields are arguments to that step.

For example, `test/integration/cases/dogstatsd-intake-metric/config.yaml` waits for ADP's UDP listener to start,
then runs these two steps:

```yaml
- step: send
  on: adp
  endpoint: dogstatsd_udp
  payload: "integration.intake.counter:3|c|#source:integration-test"
  repeat: 2
  timeout: 10s
- step: intake_has_metric
  on: intake
  name: integration.intake.counter
  metric_type: rate
  value: 0.6
  tags: [source:integration-test]
  timeout: 120s
```

The action sends two counts of 3 to the participant named `adp`. ADP exports the total as a rate: 6 counts in
10 seconds, or 0.6 per second. The assertion polls the `intake` participant's `dump` operation until it finds the
matching series or its time runs out. Neither step needs to know how its participant runs.

Panoramic runs steps in order. A `parallel` block runs assertions together and waits for them all. It cannot contain
actions or another block. If a step does not pass, panoramic leaves later procedure entries `not_run`.

Give time limits a unit such as `30s`. Most steps take their budget from `timeout`, `duration` or `during`.
The [`compare` step](#writing-a-comparison-case) uses the time left in the case. An unmet condition fails a step.
If a step does not report within its budget plus 10 seconds, panoramic marks it `errored`.
If the case timeout stops a running step, panoramic marks it `interrupted`.

To find the available steps, start at `register` in `bin/correctness/panoramic/src/steps/mod.rs`.
Each step's implementation defines its YAML fields, which fields accept [references](#references), and what it needs
from its participants.
When it plans the case, panoramic checks those fields and needs for every OS in `subject_os`. For example, it rejects
the procedure above if `adp` has no `dogstatsd_udp` endpoint or `intake` offers no `dump` operation.

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

### Checking a metric at the intake

In the [DogStatsD example](#steps), one metric series must match all the criteria. A series has a name, a set of tags
(including `host:<value>` when present) and a type. On each poll, `intake_has_metric` sums counts and rates across
flushes, merges sketches and takes the latest gauge value. Ask for the total: a partial sum can match an early poll.
`value` uses approximate equality and is not valid for sketches.

With the default `tags_match: subset`, the series can have more tags than the case names. Use `exact` to compare the
full tag sets, without regard to order or duplicates. With `exact`, `tags: []` means no tags and no host. Even an
exact match does not rule out other series. A mismatch message shows the series with that name, or other names it found.

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

### What each side must hold

A comparison answers two questions:

1. Did the fixture's output reach each intake? Two empty intakes can look equal. So can two intakes that hold only
   unrelated telemetry. Set expectations from the fixture, not from what ADP sends today. If the case samples or
   drops telemetry, choose output that should survive.
2. Does ADP's output match the Agent's? For metrics and traces, `compare` still compares the sides when an expectation
   fails. The failure lists unmet expectations by side first, then differences from the comparison. A
   [known difference](#known-differences) cannot excuse an unmet expectation.

`compare` does not see the encoding or the endpoint of a request. A side that falls back from V3 to V2 sends the same
series and sketches, so `compare` passes it. A case about the encoding must assert the request path with `intake_from`
and `path`, as `otlp-metrics-v3` and `dsd-plain-v3` do.

For logs, `logs.min_count` sets the minimum number of records per intake. If you include a `metrics` section, set
`min_series`, `series` or both, even if the step also declares a known difference:

- `min_series`: the minimum number of series that each side is expected to hold after the filter that removes
  internal telemetry.
- `series`: a list of criteria. One series of each side must meet each criterion. A criterion has a `name`, and
  optionally `tags`, `tags_match`, `metric_type` and `value`. See
  [Checking a metric at the intake](#checking-a-metric-at-the-intake).

For traces, set expectations under `traces`:

- `min_traces` and `min_spans`: the minimum numbers of traces and spans that each side is expected to hold.
- `max_traces` and `max_spans`: the maximum numbers of traces and spans that each side is expected to hold. Use them
  when a sampler drops part of the fixture. If both sides keep every trace, the sides still match, but each side fails
  its limit. A limit can't be below the minimum of the same name.
  `test/correctness/cases/otlp-traces-sampling/config.yaml` sets each limit from the count of the side that keeps more.
- `spans`: select spans by `service` and optionally by `name` (the operation name) or `resource`. Each entry needs
  at least one of `name`, `resource` or `meta`. See [Check span tags at each intake](#check-span-tags-at-each-intake).
  An entry can also check the trace chunk that holds each span. See
  [Check the sampling decision of a span](#check-the-sampling-decision-of-a-span).
- `absent`: spans that neither intake may hold. See [Check that both sides drop a span](#check-that-both-sides-drop-a-span).
- `min_stats_groups`: the minimum number of APM stats groups that each side is expected to hold. A group is one
  aggregation key.
- `require_ssi`: whether each side must hold a span with Single Step Instrumentation metadata. It defaults to `true`.
  The Agent and ADP add that metadata to the spans that they send, so the check shows that onboarding ran.

```yaml
- step: compare
  telemetry: traces
  comparison: intake-adp
  baseline: intake-agent
  traces:
    min_traces: 4
    min_spans: 12
    spans:
      - {service: api-gateway, name: opentelemetry.server.request}
      - {service: product-db, resource: SELECT * FROM products}
    min_stats_groups: 3
```

When comparing stats, set `min_stats_groups` if you set span or trace expectations or turn off `require_ssi`.
This minimum counts the groups each intake holds. In spans-only mode (`otlp_direct_analysis_mode: true`), omit
`min_stats_groups` and `require_ssi`.

For example, `test/correctness/cases/otlp-traces-stats/config.yaml` requires at least 37 stats groups in each intake.
Its `spans` entries also check that the named probe spans arrived. Neither check proves that a span counted toward
the right group. The [group comparison](#why-one-span-can-produce-two-group-differences) tests that separately.

#### Check span tags at each intake

If ADP and the Agent both leave a password in a URL, their spans can match.
`test/correctness/cases/otlp-traces-obfuscation/config.yaml` checks the scrubbed URL itself, not just whether the
intakes agree:

```yaml
spans:
  - service: probe-obf-url-full-userinfo
    name: client.request
    meta: {http.url: "https://example.com/obf/full?page=2"}
```

The service and operation name select the spans. Each intake must hold at least one, and **every** selected span
must have that `http.url`. One clean span cannot hide a second span with the password. A failure names the intake
and the entry, and shows which tags are missing or have a wrong value.

A string in `meta` means exact equality. A map can set `equals`, `not_contains` or both. For example,
`{not_contains: canary-7f3a}` rejects a tag that contains the fixture's secret. A missing tag fails this check too.

#### Check the sampling decision of a span

An intake receives each span in a trace chunk. The chunk has a sampling `priority`, a `dropped_trace` mark and tags. A
sampler that keeps every trace on both sides sends equal spans, so a comparison cannot see it.
`test/correctness/cases/otlp-traces-sampling/config.yaml` checks the decision for each probe span. A `spans` entry can
set these fields of the chunk:

```yaml
spans:
  - service: probe-sampling
    name: otlpprobe.tsamp.002.keep
    priority: 1
    dropped_trace: false
    meta: {_dd.p.dm: "-9"}
    chunk_tags: {_dd.otlp_sr: "0.50"}
```

- `priority`: the sampling priority of the chunk, for example `1` for an automatic keep.
- `dropped_trace`: whether the chunk is marked as dropped. A side still sends a dropped chunk when it holds the spans
  of an analytics event, or when error tracking standalone mode forwards a trace without an error.
- `chunk_tags`: the tags of the chunk, with the same values as `meta`: a string for an exact value, or a map of
  `equals` and `not_contains`. A missing tag fails the entry.

As for `meta`, every span that the entry selects must meet these fields. `side` applies to them too. An `absent` entry
takes none of them.

#### When only one side meets an expectation

The same case sends a SQL statement with a literal. The Agent scrubs the `db.statement` tag; ADP keeps the literal.
Here the case requires the scrubbed text on the Agent's intake:

```yaml
spans:
  - side: baseline
    service: probe-obf-sql-simple
    meta: {db.statement: "SELECT id FROM obf_simple WHERE token = ? AND n = ?"}
```

`side` belongs to the expectation. Use `baseline` for the Agent, `comparison` for ADP, or omit it to require both.
It does not excuse a difference between the intakes. A separate [known difference](#known-differences) covers
`span.meta.db.statement` for the affected services. If ADP starts meeting the expectation too, the step fails and
tells you to remove `side`. Both intakes must then keep sending the scrubbed value.

If both sides leak the same value, there is no difference for a `known_differences` entry to match. For example,
the case records the raw command both send under [issue #3451](https://github.com/DataDog/saluki/issues/3451).
That expectation records the defect, not a successful scrub. When either side fixes it, the expectation must change.

#### Check that both sides drop a span

In `test/correctness/cases/otlp-traces-conversion/config.yaml`, the fixture sends a span with a zero trace ID.
Neither intake should receive it. A valid control span from the same service must reach both intakes:

```yaml
spans:
  - {service: probe-conv-zero, name: Internal, resource: otlpprobe.conv.zero.control}
absent:
  - {service: probe-conv-zero, resource: otlpprobe.conv.zero.trace_id}
```

`absent` selects by `service` and at least one of `name` or `resource`. If either intake has a matching span, the
failure names that side. An `absent` entry takes neither `meta` nor `side`: there should be no span to inspect on
*either* side. Its control must be a `spans` entry for the same service without `side`. Otherwise a fixture that
sends nothing to one intake could look like a successful drop.

### When the intakes hold different sets

Suppose ADP drops a span and changes the resource on another. `compare` reports the missing span by service, name
and resource. It still pairs the remaining spans by span ID and reports the changed resource. It also checks APM
stats when enabled, and reports stale known differences. For metrics, it lists the series on one side only and
compares the values of the series the intakes have in common.

A split trace need not lose any spans. If ADP puts a span in a different trace, the span ID still pairs it with the
Agent's span, but `trace_id` differs. Neither `additional_span_ignore_fields` nor a known difference can hide that
field, even with a wildcard in the known difference path.

## Known differences

A correctness case compares the Agent's intake (`baseline`) with ADP's intake (`comparison`). A **gap** is a
difference between their outputs that we intend to fix. Instead of disabling the case, describe the mismatch in a
`known_differences` entry on its `compare` step. The case can then pass with that difference and still catch others.
Only `logs`, `metrics` and `traces` accept `known_differences`. Metric entries cover series types. Trace entries cover
span fields, whole spans or APM stats.

### Read a known difference

The log comparison tests in `bin/correctness/panoramic/src/steps/analysis/logs/mod.rs` use two logs with the same
message. In `known_differences_pass_and_unknown_ones_fail`, the baseline status is `info` and the comparison status
is `Info`. These are test inputs, not an open ADP gap. Here is how a case could allow the status values to differ:

```yaml
logs:
  known_differences:
    - gap: gap/status-strings
      paths: [envelope.status]
      kinds: [value]
      reason: The two logs use different capitalization for the status.
```

- **Covered:** `info` and `Info` differ at `envelope.status`. This entry allows the difference. The `reason` explains
  it to the reader; it does not restrict the rule to those two strings.
- **Unexpected:** The test logs also differ at `content.attr.k`: one holds `1`, the other `2`. The status entry does
  not cover that path. Unless another entry covers it, the step fails and reports the attribute.
- **Stale:** Suppose both logs have status `info`. The status entry no longer matches a difference, so the step
  fails. Remove the entry when the gap is fixed.

Logs, metrics and traces use the same ratchet to enforce this last rule. Without `when`, each path in an
entry must match at least one difference. With `when`, each listed value must match a difference at each path.

A test in `bin/correctness/panoramic/src/steps/analysis/ratchet.rs` shows what happens when two fixtures have a
status gap. The entry covers `envelope.status` with
`when: {fixture_id: [severity-0, severity-1]}`. If ADP fixes `severity-0`, the step reports that path as stale for
`severity-0`, even though `severity-1` still differs. Remove the fixed fixture from `when`; keep the other one.

### Choose what the entry covers

- `gap` names the defect and `reason` explains it. `issue` is its GitHub issue number.
- `paths` names the fields to match. For logs, `content.message` names the message inside the log content;
  `envelope.status` names the status on the envelope. In a dotted path, `*` matches one segment and `**` matches zero
  or more.
- `kinds` limits the kind of difference: `value` (the values differ), `missing` (baseline only) or `extra`
  (comparison only). Logs also support `location` for a field that moved. Without `kinds`, any kind matches.
- `when` limits which records the entry covers. Each named field must match one of its listed values. For logs,
  `when` reads the baseline log's fields. A request header belongs to no log, so a rule with `when` cannot cover it.
  A log or stats entry without `when` applies to every record of that kind.

### When a metric has a different type

In `test/correctness/cases/otlp-metrics-cumulative-monotonic/config.yaml`, the Agent sends
`raymond.test.cumulative.as.gauge` as a count of 30.
ADP sends its rate over ten seconds as a gauge of 3. Comparing 30 to 3 would not test either value. The case
records the type difference under `metrics`:

```yaml
known_differences:
  - gap: gap/otlp-rate-as-gauge-test-name
    issue: 3406
    paths: [type]
    when: {name: [raymond.test.cumulative.as.gauge]}
    reason: ADP reports this sum as a gauge of its rate, and the Agent reports it as a count
```

A metric entry needs an `issue` and covers only `type` for the names in `when`. If you specify `kinds`, use `[value]`.
Each intake must hold the same name and tags with exactly one type. If those types differ, `compare` leaves that pair
out of the value comparison. A missing series or an extra type is not a type difference.

The fixture sends another sum, `otlp.cumulative.monotonic`. Its count must still match. To check the values of the
pair that `compare` left out, the case uses `intake_has_metric` on each intake: count 30 on one, gauge 3 on the other.

### APM stats in an OTLP trace case

The `otlp-traces` fixture sends spans. ADP and the Agent each compute APM stats from those spans and send the stats
to their own intake. For example, a stats group can hold a hit count for a particular service and resource.
`compare` normally checks both outputs. The `otlp-traces-ottl-transform` case uses a collector as its baseline and
sets `otlp_direct_analysis_mode: true`. That option checks spans only, so the step cannot declare a stats gap.

The stats comparison checks every group, not just the first mismatch. Each entry under `traces.known_differences`
needs an `issue`. Choose the path according to what differs:

- `stats.groups`: one intake has a group that the other lacks. Use `when` to identify the group by its fields, for
  example `{service: [api-gateway]}`.
- `stats.<field>`: both intakes have the group, but a field differs. For example, `stats.hits` covers a difference
  in the hit count. A field path also covers differences inside that field, such as a value inside a sketch.

#### Why one span can produce two group differences

The `otlp-traces-stats` fixture gives each behavior it tests a distinct operation name. This makes a group easy to
find in a failure, but the name alone does not identify the group. `compare` pairs groups by their full aggregation
key. For each pair it compares hits, errors, duration and the duration sketches after merging time buckets.

For example, `otlpprobe.tstat.self_parent` names a span whose parent ID is its own span ID. The Agent counts it as a
child; ADP counts it as a root. Because `is_trace_root` is part of the key, neither group pairs with the other.
The case uses two `stats.groups` entries for [the same gap](https://github.com/DataDog/saluki/issues/3414):

- `missing`: only the Agent has the group with this name and `is_trace_root: false`.
- `extra`: only ADP has the group with this name and `is_trace_root: true`.

Each entry's `when` names the operation and its root flag. If ADP starts counting the child but still counts the root,
only the `missing` entry goes stale. A separate `span.parent_id` entry covers the field on the span itself.

### A span field or an ignored field?

For a string HTTP status code, the Agent sends both the text and the number. ADP sends only the number.
`test/correctness/cases/otlp-traces/config.yaml` records [this gap][status-issue] on the HTTP spans:

```yaml
- gap: gap/otlp-http-status-code-string
  issue: 3433
  paths: [span.meta.http.status_code]
  kinds: [missing]
  when: {service: [api-gateway]}
  reason: The Agent sends a string HTTP status code in `meta` next to the number. ADP sends the number only.
```

`span.<field>` applies to spans that the two intakes have in common, including in spans-only mode. Use `when` to
select spans by `service`, `name`, `resource` or a combination of these fields. Here, only the missing text on
`api-gateway` spans matches. The numeric status code still has to agree. If ADP starts sending the same text, the
entry goes stale. Putting the field in `additional_span_ignore_fields` instead would skip it on every span, even
after a fix.

If the outputs differ by design, compare what should agree. For example, the Agent and ADP each set a log timestamp
when they encode the log. Panoramic checks that the timestamps are within a minute of each other rather than treating
their difference as a defect.

### When only one intake receives a span

Use `span.<field>` if the spans pair and one field differs. Use `paths: [spans]` only if an open gap explains why
the entire span is on one side. For example, the Agent keeps `otlpprobe.conv.high.mixed_a` in the trace conversion
case, but ADP drops it. There is no ADP span whose fields `compare` could check.

The entry for [this gap](https://github.com/DataDog/saluki/issues/3445) sets `kinds: [missing]` and lists the affected
resources in `when`. Use `extra` for a span only ADP sends. A whole-span entry must set `kinds` to `[missing]` or
`[extra]` and select by `name` or `resource`; `service` alone is too broad. Panoramic matches `when` against the span on
the side that has it. If ADP sends the span too, the `missing` path goes stale for that resource and `compare` checks
the paired spans.

[status-issue]: https://github.com/DataDog/saluki/issues/3433
