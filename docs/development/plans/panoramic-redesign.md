# Panoramic redesign

This file is the plan for the panoramic redesign, tracked in
[DataDog/saluki#1536](https://github.com/DataDog/saluki/issues/1536). It is kept current as the work
lands, one pull request at a time. **This file is deleted in the effort's final pull request.**

## Background

Saluki, the Agent Data Plane (ADP) has a test harness binary, `panoramic`, for running integration
tests. It runs ADP, the Datadog Agent and supporting processes in Docker containers or as native
processes on several platforms, then checks the results. It grew two test modes, correctness and
integration, each with its own case format and runner. Multi-platform and Kubernetes code paths were
added inline as it grew to meet our testing needs.

## Problem statement

Each test mode has its own config schema, runner and execution path, and each platform adds its own
branches inside them. The harness knows the software it tests and the kind of test it runs, so a new
platform, a new piece of software or a new kind of assertion means changing the harness itself. The
result is code that is hard to follow and hard to extend.

A fix must not trade that for a different rigidity. Running a new script or a new container, or
putting a file somewhere new, should be a change to a case, not to the harness.

## Design overview

Panoramic has one case schema and one execution path, whatever a test checks and wherever it runs. A
case lists participants, connects them, and runs a procedure of steps. The harness does not need to
know what kind of test it is running. A comparison test is something the case builds: ADP, the
Agent, an intake for each and a load generator, then a `compare` step. A non-comparison test uses
the same schema with different participants and steps. The test suite does not determine what is or
is not possible.

A participant is data: what to run, its environment, its files and where they go, where it listens,
and which processes to observe. A participant type is a Rust function that fills in that data for
one piece of software, such as `agent-adp` or `datadog-intake`. The case's fields are laid over the
result, so anything a participant type sets, a case can set or override. A participant with no
participant type is one the case writes in full. That is how a case runs a new container or script
without requiring new Rust code.

The harness knows how to run participants, not what they are. Knowledge of a piece of software lives
in its participant type. Assertions and actions live in steps. Platform knowledge lives in four
realization backends: Linux containers, Kubernetes pods, host process groups and Windows containers.

The harness owns every address. A participant's endpoints have names, a case connects participants
by those names, and software configuration refers to an address by name instead of stating one. The
harness never reads software configuration to learn where something listens.

Everything a case will do is known before anything starts. Planning builds every participant's data
for each OS in the case's `subject_os` and checks it, so a mistake for any OS is found on any host,
and `panoramic plan` prints the result. One lifecycle then runs the plan. It creates the
participants, starts them in connection order behind readiness, runs the procedure, observes exits
and output, and reclaims what the case made.

A realization is how a participant runs: an OS and one of four enclosures, built into the harness. A
participant type supplies its data once for each realization it supports. A new arrangement of
software, such as ADP in a container of its own, is a new participant type and nothing else.

A case and its participant types become a plan before anything starts.

```text
┌─────────────────────────────────────────────────┐           ┌────────────────────────────────────────────────┐
│ Case file                                       │           │ Participant types                              │
│ participants and their connections              │           │ one Rust function per piece of software        │
│ a procedure of steps                            │           │ agent-adp, datadog-intake, millstone           │
└────────────────────────┬────────────────────────┘           └────────────────────────┬───────────────────────┘
                         │                                                             │
                         ▼                                                             ▼
                  ┏━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
                  ┃ Participant data                                                          ┃
                  ┃ what runs, environment, files, endpoints, processes to observe            ┃
                  ┃ a participant type fills it in; the fields of the case are laid over it   ┃
                  ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┳━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
                                                        │
                                                        ▼
                  ┌───────────────────────────────────────────────────────────────────────────┐
                  │ Plan                                                                      │
                  │ built and checked for every OS in subject_os, before anything starts      │
                  │ panoramic plan prints it                                                  │
                  └─────────────────────────────────────┬─────────────────────────────────────┘
                                                        │
                                                        ▼
┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ One lifecycle runs the plan                                                                                  │
│ ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐ │
│ │      Create      ├─▶│      Start       ├─▶│    Procedure     ├─▶│     Observe      ├─▶│     Reclaim      │ │
│ │   places files   │  │ behind readiness │  │  runs the steps  │  │ exits and output │  │  on every path   │ │
│ └──────────────────┘  └──────────────────┘  └──────────────────┘  └──────────────────┘  └──────────────────┘ │
│                                                                                                              │
└───────────────────────────────────────────────────────┬──────────────────────────────────────────────────────┘
                                                        │
                                                        ▼
┌──────────────────────────────────────────────────────────────────────────────────────────────────────────────┐
│ Four realization backends do the work                                                                        │
│ ┌──────────────────────┐    ┌──────────────────────┐    ┌──────────────────────┐    ┌──────────────────────┐ │
│ │   Linux container    │    │    Kubernetes pod    │    │  Host process group  │    │  Windows container   │ │
│ └──────────────────────┘    └──────────────────────┘    └──────────────────────┘    └──────────────────────┘ │
│                                                                                                              │
└──────────────────────────────────────────────────────────────────────────────────────────────────────────────┘
```

The plan is complete before the lifecycle begins, so a mistake in a case is found without starting
anything.

## Example cases

This is the correctness case `dsd-plain`, trimmed: its config files, environment, timeouts and its
other waiting steps are left out.

```yaml
timeout: 20m
subject_os: [linux]
participants:
  adp:
    type: agent-adp
    subject: true
    connect: {intake: intake-adp.api}
  agent:
    type: agent
    connect: {intake: intake-agent.api}
  intake-adp: {type: datadog-intake}
  intake-agent: {type: datadog-intake}
  load:
    type: millstone
    connect: {comparison_target: adp.dogstatsd_uds, baseline_target: agent.dogstatsd_uds}
procedure:
  - {step: log_contains, on: load, process: comparison, pattern: Millstone finished sending}
  - {step: compare, telemetry: metrics, comparison: intake-adp, baseline: intake-agent}
```

Nothing here tells the harness it is running a comparison. ADP and the Agent each send to their own
intake, and millstone sends both the same DogStatsD load. The `compare` step then checks that the
two intakes received the same metrics. The Agent is the oracle here, and case files call it the
baseline. (We will not be changing established terminology here; continue to call the Agent
*baseline* and ADP the *comparison*). Millstone's connections use the same roles:
`comparison_target` and `baseline_target`.

A case can also bring software the harness has never heard of. Here `gen` has no participant type,
and `adp` tells its OTLP receiver where to listen by referring to the endpoint.

```yaml
  adp:
    type: agent-adp
    subject: true
    env:
      DD_DATA_PLANE_OTLP_ENABLED: "true"
      DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY: "0.0.0.0:{{ self.endpoints.otlp_grpc.port }}"
  gen:
    image: ghcr.io/open-telemetry/opentelemetry-collector-contrib/telemetrygen:<tag>
    args:
      - metrics
      - --otlp-insecure
      - --otlp-endpoint
      - "{{ self.connections.target.host }}:{{ self.connections.target.port }}"
    connect: {target: adp.otlp_grpc}
```

`gen` starts once `adp.otlp_grpc` accepts connections, sends its load and exits. Nothing in Rust
changes.

## Structure

Panoramic is one crate with three modules, plus one small helper crate. The harness core depends on
nothing that knows what participants run. Participant types and steps do not depend on each other;
they meet only through the core. One rule in `test/architecture` enforces both.

| Part | Owns | Depends on |
| --- | --- | --- |
| `panoramic` binary | CLI, case selection, checking images and programs before a run, kind setup, scheduling and reports; registers participant types and steps | `core`, `participants`, `steps` |
| `core` module | Case loading, planning, references, the lifecycle, realization backends, handles and observation | `airlock`, `panoramic-courier` |
| `participants` module | Participant types: what each piece of software runs and how | `core` |
| `steps` module | Assertions and actions, including `compare` | `core`, `stele` |
| `panoramic-courier` crate | Probes, sends, HTTP requests and file follows from inside a container or pod | none |

The courier is its own program. The harness copies it into each container and pod and runs it there,
because a probe from outside cannot tell whether anything is listening.

## Running cases

What runs, and where, follows from the case files. `panoramic run -d <dir>` selects every case in
the directory for one subject OS, which defaults to the host's. A case whose `subject_os` leaves
that OS out is reported as skipped. `panoramic plan <case> --subject-os <os>` prints, as JSON,
everything a case would do on that OS, without running anything.

A case or a participant type names each image and host program by reference, and a run can replace
any of them, which is how CI points cases at the images a pipeline built. Before any case starts,
panoramic checks that each one exists and pulls an image the Docker daemon lacks. A case whose
realization the host cannot provide, such as pods without a kind cluster (Kubernetes in Docker), is
reported as errored before the run starts.

Cases in containers and pods run in parallel. Cases whose participants are host processes run one at
a time, because they share the host's ports.

CI uses the same commands. It generates one correctness job per case from `panoramic list --json`,
using the kind template when a case uses pods. Integration cases run as one job per OS.

## Trade-offs

- **Size of the change.** It replaces the legacy runners, their case schemas and every case file,
  and landing it takes many PRs and a lot of review.
- **Openness moves some errors later.** A case can replace a participant's command or put a file
  where it breaks the software. That fails when the case runs, not when it loads. One guard covers
  the failure that would otherwise be silent.
- **Addresses by reference.** Software configuration that sets a listener's address must refer to
  the endpoint, and the harness does not know whether the software opened the listener. The
  fallback, if this cannot be built, is to read each address from the case's configuration.
- **Cost per platform.** A participant type is cheap when its software runs the same everywhere, and
  grows with each platform where it differs. A new realization costs a backend, then data from every
  participant type that should run there.
- **Not built yet.** Containers beside a host-process subject, and assigned ports. Until then a
  helper on macOS is a host program, and host-process cases run one at a time.
- **Steps that name capabilities.** `compare` and the intake steps need a participant that offers
  `dump`. A new kind of participant may need steps of its own, or a script the case supplies.

## Implementation Details

This section gives the specifications an implementation needs. Code excerpts show shape and names,
not final signatures.

### The case schema

A case is a directory with a `config.yaml` and the files it references. Every key has one owner: the
harness core, a participant type (the keys under `with`) or a step (its own fields). A key its owner
does not know is an error before anything starts.

```yaml
description: <text>                    # optional
timeout: <duration>                    # required, with a unit: 20m, 180s
subject_os: [linux, macos, windows]    # required, at least one
participants:
  <name>: <participant>                # exactly one has subject: true
procedure:
  - <step>
  - parallel: [<step>, ...]            # assertions only, no nesting
```

The directory names the case. A participant name is a lowercase DNS label, because it becomes a
container name, a pod name and a host name. `localhost`, a name of only digits and a hexadecimal number
written with `0x` are errors, because a resolver answers them with an address that is not the
container's.

**Participant fields.** An entry is flat. The first six fields say where the participant sits in the
case. The rest are the participant's data, and a participant type fills in the same ones.

| Field | Value | Meaning |
| --- | --- | --- |
| `type` | a participant type name | Optional. Without it the case writes the participant in full. |
| `subject` | `true` | The participant under test. Exactly one sets it. |
| `realization` | `{enclosure, options}` | Written only when the realization is under test. Otherwise the subject gets its OS's default and every other participant follows the subject's enclosure, and not its options. |
| `connect` | `{<name>: <participant>.<endpoint>}` | The endpoints this participant consumes. |
| `with` | a map | The participant type's own parameters, such as `host_view: true`. An error without `type`. |
| `os` | `{<subject OS>: {<fields>}}` | Fields that differ by subject OS. |
| `image`, `command`, `args` | text, list, list | What runs in a container or pod. `command` replaces the image's entrypoint and `args` its default arguments, as in Kubernetes. |
| `programs` | a list of `{name, path, args, env}` | What runs as host processes. Each program is one observed process. |
| `env` | `{NAME: "text"}` | Environment of every process of the participant. |
| `files` | `{<destination or name>: <source>}` | Files placed before start. |
| `endpoints` | `{<name>: {<protocol>: <address>}}` | Where the participant listens. |
| `ready` | `{<endpoint>: <evidence>}` | Readiness evidence, where the default for the endpoint's protocol is not enough. |
| `processes` | a list | The processes steps may name, and how each is observed. |
| `commands` | `{<name>: [<program>, <arg>, ...]}` | Command lines of the participant's software, for `exec`. |
| `mounts`, `collect` | lists | Host paths mounted in; files copied out after the procedure. |

The realization decides which shape of "what runs" applies. `image` on a host process group, or
`programs` in a container, is an error. A program's `path` is found under the participant's own
directory when written `./x`, on the runner host's `PATH` when it is a bare name, and as written
otherwise. A bare name is a host requirement.

**Field syntax.**

```yaml
realization: {enclosure: kubernetes-pod, options: {host_pid: true, cri_socket: true}}
# enclosures: linux-container, windows-container, kubernetes-pod, host-process-group
# options: host_cgroup_namespace on a Linux container; host_pid and cri_socket on a pod

with: {host_view: true}       # agent-adp, agent: mount the host's /proc, cgroup tree and container
                              # runtime socket, and point the Agent at them. This is the host view.
with: {own_ip: {env: [DD_BIND_HOST], hosts: [foo.local]}}
                              # agent-adp: set these variables to the container's address on the case's network, and
                              # map these host names to it, before the software starts

programs:
  - {name: main, path: python3, args: [./helper/serve.py], env: {PYTHONUNBUFFERED: "1"}}

endpoints:
  api:    {http: 8080}
  secure: {http: 8443, tls: true}
  stats:  {udp: 8125}
  docker: {unix_stream: "{{ self.dir }}/docker.sock"}
  pipe:   {named_pipe: my-pipe}

ready:                        # accepts | exists | none | {http: <path>} | {output: <pattern>, process: <name>}
  api: {http: /ready}

processes:                    # exit: container | {record: <file>}; output: container | {file: <file>}
  - {name: adp,   exit: {record: data-plane}, output: {file: adp.log}}
  - {name: agent, exit: {record: agent},      output: {file: agent.log}}

commands:
  psql: [psql, -U, postgres]                        # a list, or {argv, env}
  adp:  {program: adp, argv: [-c, datadog.yaml]}    # as host processes: one of the participant's programs

mounts:
  - {host: /proc, path: /host/proc, read_only: true}    # Linux containers and pods only

collect:
  - {path: /var/lib/millstone/baseline/input.jsonl.zst, name: input.jsonl.zst, keep: on_failure}
```

Participant types set `ready`, `processes`, `commands`, `mounts` and `collect` today, and no case
does. A case may set them because a case's fields are the spec's fields. A `ready` entry a case
writes for an endpoint nothing connects to is an error.

**Files.** A key that looks like a path is a destination. Any other key must be a name the
participant type declares.

| Realization | A destination is |
| --- | --- |
| Linux container, pod | an absolute path |
| Windows container | a path on drive `C:`, written with `/` or `\` |
| Host process group | `./…`, under the participant's own directory, with no `..` |

A destination that ends in a separator is a directory. A value is a source path relative to the case
directory, or `{from, template, mode}`. `template: true` marks a UTF-8 file whose references the
harness resolves; every other file is copied byte for byte. `mode` is a quoted octal string such as
`"0755"`. The default is `"0644"`, an unquoted number is an error, and `mode` is an error on a
Windows participant.

A participant type may name the configuration files of its software. A name stands for one
destination on one realization, and the printed plan shows it. A path key equal to a name's
destination is an error that says to use the name, so each file has one spelling.

| Participant type | Name | Destination |
| --- | --- | --- |
| `agent-adp`, `agent` | `datadog.yaml` | `/etc/datadog-agent/datadog.yaml` in a Linux container or pod |
| `agent` | `otel-config.yaml` | `/etc/datadog-agent/otel-config.yaml` |
| `millstone` | `millstone.yaml` | `/etc/millstone/comparison.yaml` |

No case supplies `datadog.yaml` on macOS or Windows today. The name is declared there when one does.

**The `os` overlay.** An `os` block may sit on a participant and on a step. Its keys are subject OS
names, and each value holds fields for that OS. This is how a command, a path or an image differs
between platforms.

```yaml
participants:
  adp:
    type: agent-adp
    subject: true
    os:
      linux:   {files: {/dogstatsd-top/: linux/}}
      windows: {files: {"C:/dogstatsd-top/": windows/}}
procedure:
  - step: exec
    on: adp
    timeout: 90s
    os:
      linux:   {command: [/opt/datadog-agent/embedded/bin/python3, /dogstatsd-top/run_dogstatsd_top_test.py]}
      windows: {command: [pwsh, -NoProfile, -File, 'C:\dogstatsd-top\run_dogstatsd_top_test.ps1']}
```

On a participant the overlay may hold `with` and any data field. It may not hold `type`, `subject`,
`realization`, `connect` or `os`. A key that is not in `subject_os` is an error. The overlay is
keyed by subject OS and not by realization: each OS has one set of paths today. A second realization
on one OS with different paths is the moment to add a key.

**Checked before anything starts.** `list`, `plan` and `run` all load every case they discover and
plan it for every OS in its `subject_os`, not only the selected one. So a field that cannot be
honored on a declared OS is an error on any host.

- Every key is known to its owner. There is exactly one subject.
- Every file source exists and stays inside the suite tree. Every `{{` starts a well-formed
  reference.
- Every participant has a realization its participant type supports, and `with` decodes.
- The launch shape fits the realization.
- Every file destination is valid for the realization, and no two entries land on one path.
- No two endpoints of one participant share an address.
- Every `connect` target exists, has a route, and is used.
- Every reference names something that exists, in a place where its value will be known in time.
- Create order and start order have no cycle.
- Every step decodes, and every participant, process, command, operation and endpoint it names
  exists on that OS.

Three things cannot be checked before start: that a program exists in an image, that a script is
correct, and that software opens the listeners it was told to. Those show up as a failed start, a
failed `exec` or a readiness timeout, with the output attached.

### The participant spec and the participant type

`ParticipantSpec` is the participant as data. It is the same struct on every realization, it is what
a participant type returns, and a case's participant entry decodes into the same fields.

```rust
//! core::spec

/// One participant as data: what runs, what it is given, where it listens and how it is observed.
/// Every field may be left unset, which says nothing at that layer.
#[derive(Clone, Debug, Default)]
pub struct ParticipantSpec {
    // What runs, container shape.
    pub image: Option<String>,              // a reference; a run may replace it by this text
    pub command: Option<Vec<Template>>,     // replaces the image's entrypoint
    pub args: Option<Vec<Template>>,        // replaces the image's default arguments
    // What runs, host shape.
    pub programs: Option<Vec<Program>>,     // in start order; each is one observed process

    pub env: BTreeMap<String, Template>,
    pub files: BTreeMap<String, FileSpec>,  // keyed by destination; a case may also key by alias
    pub mounts: Option<Vec<HostMount>>,
    pub endpoints: BTreeMap<String, EndpointSpec>,
    pub ready: BTreeMap<String, Evidence>,
    pub processes: Option<Vec<ProcessSpec>>, // unset: one process, `main`, observed through the container
    pub commands: BTreeMap<String, CommandSpec>,
    pub collect: Option<Vec<CollectFile>>,

    // Only a participant type declares these four. A case uses them and cannot define them.
    pub aliases: BTreeMap<&'static str, &'static str>,  // file names a case may write, one destination each
    pub consumes: BTreeMap<&'static str, Consumed>,     // connection names it knows, and what each delivers
    pub operations: &'static [&'static dyn Operation],  // capabilities steps call by name
    pub wiring: BTreeMap<String, Wiring>,              // by endpoint: why a default was withdrawn, the line to write
}

pub struct Program {
    pub name: String,                       // the process name steps use; `main` by default
    pub path: String,                       // a reference, like `image`
    pub args: Vec<Template>,
    pub env: BTreeMap<String, Template>,    // this program only, over the participant's env
}

pub struct FileSpec {
    pub source: FileSource,   // Case { from, template } | Bytes { label, bytes } | Text { label, text }
    pub mode: Option<u32>,    // None is 0644; an error on a Windows participant
    pub supervises: bool,     // part of what writes exit records or output files; only a participant type sets it
}

pub struct EndpointSpec { pub protocol: Protocol, pub address: Address, pub tls: bool }
pub enum Address { Port(u16), Path(Template), Pipe(String) }

pub struct ProcessSpec { pub name: String, pub exit: ExitChannel, pub output: OutputChannel }
pub enum ExitChannel { Container, Record { file: String } }
pub enum OutputChannel { Container, File { file: String } }

pub struct CommandSpec {
    pub program: Option<String>,            // as host processes: run this named program of the participant
    pub argv: Vec<Template>,
    pub env: BTreeMap<String, Template>,
}

pub struct Consumed {
    pub accepts: &'static [Protocol],
    pub required: bool,
    /// Environment added when the case connects it, as template text:
    /// ("DD_DD_URL", "{{ self.connections.intake.url }}").
    pub env: &'static [(&'static str, &'static str)],
}
```

A participant type is a Rust definition with one job: fill in a `ParticipantSpec` for one piece of
software on one realization.

```rust
//! core::participant

pub trait ParticipantType: Send + Sync {
    /// The name a case writes as `type`.
    fn name(&self) -> &'static str;

    /// The (participant OS, enclosure) pairs this participant type has a spec for.
    fn supports(&self) -> &'static [(Os, Enclosure)];

    /// Fills in the spec for one participant on one realization.
    ///
    /// Core calls this at plan time, once per case and declared subject OS. It does no I/O.
    /// Core then lays the case's own fields over the result, so it holds defaults, not final
    /// values. It may hold references; core resolves them when their values exist.
    ///
    /// # Errors
    ///
    /// Returns an error for a `with` parameter, an option or a case value this participant type
    /// cannot honor on this realization.
    fn spec(&self, cx: &SpecContext<'_>) -> Result<ParticipantSpec, String>;
}

/// What a participant type may read while it fills in a spec.
pub struct SpecContext<'a> {
    pub realization: Realization,
    pub options: &'a BTreeMap<String, bool>,   // realization options the case set
    pub case: &'a ParticipantSpec,             // what the case wrote for this OS, references unresolved
    pub connect: &'a BTreeMap<String, ConnectionTarget>,
    pub(crate) with: &'a BTreeMap<String, serde_json::Value>,
    pub(crate) sources: &'a CaseSources,       // the case's files, read once at load
}

impl SpecContext<'_> {
    /// Decodes `with` strictly into the participant type's own struct.
    pub fn with<T: DeserializeOwned>(&self) -> Result<T, String>;
    /// The text of the file the case supplies under a name the participant type declares, as
    /// written. `None` when the case supplies no such file.
    pub fn file_text(&self, name: &str) -> Result<Option<&str>, String>;
}
```

| A participant type may | It may not |
| --- | --- |
| Read the case's `env` keys and literal values, to choose defaults | Read a resolved address, directory or identity: none exists yet |
| Read the text of a file the case supplies | Change what the case wrote: the case's fields are laid over its result |
| Reject what it cannot honor on this realization | Read another participant |
| Return references anywhere a case could write one | Do I/O |

The largest participant type shows the shape. This is `agent-adp` in a Linux container, abbreviated:

```rust
fn linux_container(cx: &SpecContext<'_>) -> Result<ParticipantSpec, String> {
    let with: With = cx.with()?;            // host_view, own_ip
    let mut spec = ParticipantSpec {
        image: Some("saluki-images/datadog-agent:testing-devel".into()),
        // No command: the image's own entrypoint starts s6.
        env: default_env(),                 // DD_DOGSTATSD_PORT: "{{ self.endpoints.dogstatsd_udp.port }}", ...
        endpoints: endpoints(cx.realization),
        ready: ready(),
        aliases: BTreeMap::from([("datadog.yaml", "/etc/datadog-agent/datadog.yaml")]),
        files: s6::overlay(),               // finish, run and log/run per service; 0755; supervising
        processes: Some(s6::processes()),   // adp: record data-plane, file adp.log; agent: record agent, file agent.log
        consumes: BTreeMap::from([("intake", INTAKE)]),
        commands: commands("/opt/datadog-agent/embedded/bin/agent-data-plane", "/opt/datadog-agent/bin/agent/agent"),
        operations: OPERATIONS,
        ..ParticipantSpec::default()
    };
    if with.host_view {
        // three read-only host mounts and the environment that points the Agent at them
    }
    withdraw_defaults(cx, &mut spec.env)?;  // two rules, under "Endpoints and wiring"
    Ok(spec)
}
```

Two points matter when the existing participant types are written this way. `millstone` builds its
second sender file by renaming one connection in the case's file text, before anything resolves. A
container's own IP cannot be a reference in its own environment, so `agent-adp` takes
`with: {own_ip: {env: [...], hosts: [...]}}` and writes the value from inside the container.

No participant type code runs after plan compile, except an operation a step calls. The plan holds
everything the lifecycle acts on, as "Planning and `panoramic plan`" describes.

### Merging and guards

A participant has three layers: the participant type's spec, the case's fields, and the case's
`os.<subject OS>` fields. A step has two: its fields and its `os.<subject OS>` fields. One rule
joins all of them.

1. A field that is a map merges by key. An entry in the upper layer replaces the entry with the same
   key below, whole. `env`, `files`, `endpoints`, `ready`, `commands` and `with` are maps.
2. Any other field replaces the field below, whole: `image`, `command`, `args`, `programs`,
   `processes`, `mounts` and `collect`.

Merging goes one level into a field and no deeper. To change one endpoint or one file, a layer
restates that entry. Nothing can be removed, only replaced. One addition applies to a participant: a
layer that sets `command` and not `args` removes the `args` below, as in Kubernetes, so a new
program does not inherit the old program's arguments.

```rust
impl ParticipantSpec {
    /// Lays `over` on top of `self`: maps merge by key, every other field replaces.
    pub(crate) fn lay(&mut self, over: ParticipantSpec) {
        if over.command.is_some() && over.args.is_none() {
            self.args = None;
        }
        replace(&mut self.image, over.image);
        replace(&mut self.command, over.command);
        replace(&mut self.args, over.args);
        replace(&mut self.programs, over.programs);
        replace(&mut self.mounts, over.mounts);
        replace(&mut self.processes, over.processes);
        replace(&mut self.collect, over.collect);
        self.env.extend(over.env);
        self.files.extend(over.files);
        self.endpoints.extend(over.endpoints);
        self.ready.extend(over.ready);
        self.commands.extend(over.commands);
    }
}
```

Building one participant's spec for one subject OS takes five steps:

1. Lay the case's `os` fields over the case's fields. This is the case layer. `with` is not part of
   the spec: the case's `with` and its overlay's `with` merge by key here, before the participant
   type reads them.
2. Call the participant type's `spec`, which may read the case layer. A participant with no
   participant type starts from an empty spec.
3. Rewrite each name in the case layer's `files` to its destination. A Windows destination is
   normalized to `\` first, so two spellings of one path are one key. A participant type keys its
   own `files` by destination only, so after this step both layers use one key for one file.
4. Check the guards.
5. Lay the case layer over the participant type's spec.

The general participant is this path with an empty lower layer. It has no definition and no code of
its own.

**Guards.** Openness lets a case replace anything a participant type set. Two replacements would
fail silently, so they are errors.

- **Supervision.** A spec is supervised when one of its processes reports through a record file or
  an output file, because something inside the container writes those. If a case sets `command` or
  `args` on such a participant, or places a file where the participant type has a file it marks as
  supervising, the case must also set `processes`. Otherwise an exit record is never written, the
  process reads as still running, and `process_stable_for` could pass on a process that never
  started. The guard covers `agent-adp` and `agent` under s6, `agent-adp` in a Windows container,
  and `millstone`.
- **Delivered keys.** A case may not set an environment key that one of its connections delivers, in
  `env`, in a program's `env` or in a command's `env`. Each would win over the delivered value.

Two checks apply to every merged spec: an `env` key with the prefix `PANORAMIC_`, in any letter
case, is reserved, and no file may land on a path the realization reserves: the courier's path, and
the participant's exit and log directories.

Implementation note: serde cannot combine `deny_unknown_fields` with `flatten`, so the participant
entry, the overlay and the spec each list the shared fields. One test checks that a field added to
one is in all three.

### References

A reference is a name for a value the harness owns. It is written `{{ … }}` inside text, and the
harness replaces it when the value exists.

```
reference = "{{" path "}}"
path      = holder ".endpoints." name "." ( "port" | "path" | "name" )
          | "self.connections." name "." ( "url" | "host" | "port" | "path" )
          | holder "." ( "dir" | "ip" | "container_id" | "pod_uid" )
holder    = "self" | participant-name
```

`self` is the participant whose field or file holds the reference. A step has no `self`: a step
field names the participant. Every `{{` starts a reference, and there is no escape.

| Reference | Value |
| --- | --- |
| `<p>.endpoints.<e>.port` | The port the endpoint listens on. |
| `<p>.endpoints.<e>.path` | The socket path, or `\\.\pipe\<name>` for a named pipe. |
| `<p>.endpoints.<e>.name` | A named pipe's bare name. |
| `self.connections.<c>.url` | The connected endpoint's address as this participant reaches it. |
| `self.connections.<c>.host`, `.port` | The same, for `tcp`, `udp`, `http` and `grpc`. |
| `self.connections.<c>.path` | The same, for Unix sockets. |
| `<p>.dir` | The participant's own directory. In Linux containers and pods of one case it is on storage every participant sees at the same path. In a Windows container it is inside that container only. As host processes it is the working directory and the root of `./` destinations. |
| `<p>.ip`, `<p>.container_id`, `<p>.pod_uid` | The participant's address on the case's network, and its runtime identities. |

The two address namespaces answer different questions. `endpoints` gives what does not depend on who
is looking, which is how a participant tells its own software where to listen. `connections` gives
the whole address as the consumer reaches it, which depends on both realizations. In its own fields
and files a participant references only its own endpoints. To use another participant's address it
connects and writes `self.connections`, which keeps the start order and the readiness wait attached
to every use. A step field may reference any participant's endpoint.

**When a value exists**, by the realization of the participant it belongs to:

| Value | Linux container | Windows container | Pod | Host process group |
| --- | --- | --- | --- | --- |
| `endpoints`, `connections`, `dir` | plan | plan | plan | plan |
| `ip` | after start | after start | after start | plan: `127.0.0.1` |
| `container_id` | after create | after create | after start | none |
| `pod_uid` | none | none | after start | none |

**When a place is fixed:**

| Place | Fixed |
| --- | --- |
| A socket path in `endpoints` | at plan; it may refer only to `self.dir`, because the address is built from it |
| `command`, `args`, `programs`, `env`, a pattern in `ready` | when the participant is created |
| A template file in a container or as host processes | when the participant is created |
| A template file in a pod | when the pod is created, or after it starts if the file holds a value that exists only then |
| `commands`, and the template fields of a step | when the step runs |
| `image`, a program's `path`, a `files` key, `with`, `mounts`, `collect`, `processes` | never hold references |

**The rule:** a reference is accepted in a place when its value exists by the time the place is
fixed. Anything else is a plan error. Three consequences:

- A container's own IP cannot be written into its own environment, because the environment is fixed
  at create and the address exists after start. Where software needs it, the participant type
  provides it from inside the container.
- A pod's template file that holds an after-start value is written by the pod backend once the
  values exist, and the pod's command waits for it. Only such a pod waits.
- A value substituted into a `regex: true` pattern is escaped, so a path or an address matches
  itself.

**References decide order.** A participant is created after every participant whose `container_id`
it references in a place fixed at create. A participant starts after every participant it connects
to, once the endpoint's evidence holds. The two orders are separate on purpose:
`liveness-container-tags` needs a helper created after the subject and started before it. A cycle in
either is a plan error that names the chain.

Core has one function that visits every piece of text in a spec that may hold a reference, with the
place it is in, and one resolver. Resolution runs with the same code at plan compile, when a
participant is created, and when a step runs, each time with more values known. The plan records for
each template when its last reference becomes known, so creation never meets one it cannot fill.

### Endpoints and wiring

An endpoint is a name, a protocol and an address. The address is the part the harness owns: a port, a
socket path or a pipe name. Which interface a listener binds is software configuration. It stays in
the case and the harness never reads it.

| Protocol | Address | URL a consumer receives | Default readiness evidence |
| --- | --- | --- | --- |
| `tcp` | port | `tcp://<host>:<port>` | the port accepts a connection |
| `udp` | port | `udp://<host>:<port>` | none: a probe cannot confirm a UDP listener |
| `http` | port | `http://<host>:<port>`, or `https://` with `tls: true` | the port accepts a connection |
| `grpc` | port | `grpc://<host>:<port>` | the port accepts a connection |
| `unix_datagram` | path | `unixgram://<path>` | the socket exists |
| `unix_stream` | path | `unix://<path>` | the socket exists |
| `named_pipe` | pipe name | none: no participant can connect to a pipe | none |

**A participant type declares a table and wires its defaults.** Its `spec` returns its endpoints as
data, and a default environment. A default environment entry that carries a listener's address is
written as a reference. That reference is the wiring.

```rust
// agent-adp, every realization. Each entry: the environment key, the datadog.yaml key for the
// same setting, and the value. Resolved, the values equal the ports the harness injects today.
const DEFAULT_ENV: &[DefaultEnv] = &[
    env("DD_DOGSTATSD_PORT", "dogstatsd_port", "{{ self.endpoints.dogstatsd_udp.port }}"),
    env("DD_APM_RECEIVER_PORT", "apm_config.receiver_port", "{{ self.endpoints.apm_tcp.port }}"),
    env("DD_DATA_PLANE_API_LISTEN_ADDRESS", "data_plane.api_listen_address",
        "tcp://0.0.0.0:{{ self.endpoints.api.port }}"),
    env("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT", "otlp_config.receiver.protocols.grpc.endpoint",
        "0.0.0.0:{{ self.endpoints.agent_otlp_grpc.port }}"),
    env("DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT", "otlp_config.receiver.protocols.http.endpoint",
        "0.0.0.0:{{ self.endpoints.agent_otlp_http.port }}"),
    // The remaining keys keep listeners no case talks to off the software's default ports.
    // They stay literals and are not endpoints: DD_CMD_PORT "45001", DD_GUI_PORT "-1", ...
];
```

An endpoint is wired by default when the default environment references it. Otherwise it is opt-in:
the table gives it an address, and a case that wants the listener writes the reference under the key
the software reads. A listener is wired by default only if the harness already sets its key for
integration cases today, so an integration case keeps its environment when it says nothing. In a
comparison case both sides gain these keys: agent has the same default environment without the
`DD_DATA_PLANE_*` keys, which are ADP's and name endpoints only `agent-adp` has, each entry
referencing its own endpoint of the same name. The harness sets these keys only for integration
cases today. That is a named change, shown by the configuration diff.

`agent-adp` declares these endpoints.

| Endpoint | Protocol | Address | Where | Wiring |
| --- | --- | --- | --- | --- |
| `dogstatsd_udp` | `udp` | 48125 | every realization | default, `DD_DOGSTATSD_PORT` |
| `apm_tcp` | `http` | 48126 | every realization | default, `DD_APM_RECEIVER_PORT` |
| `api` | `http` | 45100 | every realization | default, `DD_DATA_PLANE_API_LISTEN_ADDRESS` |
| `agent_otlp_grpc` | `grpc` | 44317 | every realization | default, `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_GRPC_ENDPOINT` |
| `agent_otlp_http` | `http` | 44318 | every realization | default, `DD_OTLP_CONFIG_RECEIVER_PROTOCOLS_HTTP_ENDPOINT` |
| `otlp_grpc` | `grpc` | 46317 | every realization | opt-in, `DD_DATA_PLANE_OTLP_RECEIVER_GRPC_ENDPOINT_TEMPORARY` |
| `otlp_http` | `http` | 46318 | every realization | opt-in, `DD_DATA_PLANE_OTLP_RECEIVER_HTTP_ENDPOINT_TEMPORARY` |
| `dogstatsd_tcp` | `tcp` | 9126 | every realization | opt-in, `DD_DOGSTATSD_TCP_PORT` |
| `dogstatsd_uds` | `unix_datagram` | `{{ self.dir }}/dogstatsd_uds.sock` | not Windows | opt-in, `DD_DOGSTATSD_SOCKET` |
| `dogstatsd_uds_stream` | `unix_stream` | `{{ self.dir }}/dogstatsd_uds_stream.sock` | not Windows | opt-in, `DD_DOGSTATSD_STREAM_SOCKET` |
| `apm_uds` | `unix_stream` | `{{ self.dir }}/apm_uds.sock` | Linux only | opt-in, `DD_APM_RECEIVER_SOCKET` |
| `dogstatsd_pipe` | `named_pipe` | `panoramic-dogstatsd` | Windows only | opt-in, `DD_DOGSTATSD_PIPE_NAME` |

When a case sets `DD_DATA_PLANE_ENABLED` and `DD_DATA_PLANE_OTLP_ENABLED` to `true`, the image's
`cont-init.d` script moves the Agent's OTLP listeners to `127.0.0.1:14317` and `127.0.0.1:14318`, so
`agent_otlp_grpc` and `agent_otlp_http` are unwired whatever the case writes: connecting to one or waiting
on it is a plan error.

For an opt-in endpoint the key is the case's choice. `ADP_DD_<key>` in a Linux container reaches ADP
only, and the `datadog.yaml` spelling works wherever the case supplies that file.

The addresses of the endpoints wired by default are the values
`bin/correctness/panoramic/src/test_env.rs` injects on `main` today, all below 49152, and the
default environment is a copy of that file's, checked against it after every rebase (see "How old
and new coexist"). The opt-in ports are the ones integration cases write today.
Comparison cases write different ones (54317 and 4317), and they move to the table's. A few case
scripts hold these ports as literals. They keep working while addresses are fixed, and a script
should take them from its `exec` step's `env` by reference before addresses are ever assigned.

The other participant types are smaller.

| Participant type | Endpoints | Wiring |
| --- | --- | --- |
| `agent` | `dogstatsd_udp` 8125, `apm_tcp` 8126, `agent_otlp_grpc` 44317, `agent_otlp_http` 44318, `otel_otlp_grpc` 4317, and the three sockets as above | as agent-adp, without the `DD_DATA_PLANE_*` keys: the baseline runs no data plane, and both sides of a comparison set the same Agent keys |
| `datadog-intake` | `api` (`http`, 2049), `dogstatsd_forward` (`udp`, 9125) | both by default, through `INTAKE_HTTP_LISTEN_ADDR` and `INTAKE_DOGSTATSD_LISTEN_ADDR` |
| `millstone` | none |  |

`agent` keeps the Agent's own ports for DogStatsD and APM while it runs only in its own container.
Before `agent` runs as host processes, `hostproc` moves them off those ports, as `test_env.rs` does
for `agent-adp`, so it cannot collide with an Agent installed on the host.

The intake binary hard-codes its two ports on `main` today. Reading these two variables is a small
change to the intake that lands with its participant type, and the tools image is rebuilt with it.

`agent-adp` and `agent` share endpoint names on purpose. A comparison case usually gives the
comparison and the baseline the same `datadog.yaml`, and a reference in a shared file must resolve
on both.

**A case declares or restates an endpoint.** `endpoints` is a field of every participant. This is
how a participant with no participant type has endpoints, and how a case reaches a listener a
participant type does not know yet.

```yaml
participants:
  docker-api:
    image: python:3.12-slim
    command: [python3, /case/fake_docker.py, "{{ adp.container_id }}", "{{ self.endpoints.api.path }}"]
    files: {/case/: fixture/}
    endpoints:
      api: {unix_stream: "{{ self.dir }}/docker.sock"}
  db:
    image: postgres:16
    endpoints:
      sql: {tcp: 5432}          # the image listens here without being told
```

An entry is one protocol with its address, plus `tls: true` where an `http` listener serves TLS. A case
that restates an endpoint a participant type declares replaces the entry whole, and should do so
only when that address is itself under test.

**One address, one endpoint.** Two endpoints of one participant cannot share an address, because
only one listener can hold it. Every endpoint counts, wired or not. The ports of `tcp`, `http` and
`grpc` endpoints are one space, and the ports of `udp` endpoints are another. A socket path is one
space for both Unix protocols. Pipe names are their own space, compared without letter case as Windows
does. Planning compares addresses after it fills their references, and a shared address is a plan
error that names both endpoints. A restated endpoint is the usual cause:

```
participant 'adp': endpoints 'agent_otlp_grpc' and 'api' share tcp port 45100.
  Each endpoint of a participant needs an address of its own.
```

**The wired check.** An endpoint a participant type declares is usable only if the participant's own
final configuration references it: its `env` after the merge and the withdraw rule, its `command`,
`args` or `programs`, or a file it places with `template: true`. An endpoint the case declares is
usable as declared, because the case is the authority for a command it supplies. Using an unwired
endpoint in a `connect`, a step or a readiness gate is a plan error, and the error says why. The
participant type's `wiring` gives the withdrawn default and the line to write:

```
participant 'adp' (agent-adp, linux-container): endpoint 'dogstatsd_udp' is not wired.
  datadog.yaml sets 'dogstatsd_port', so the default DD_DOGSTATSD_PORT was withdrawn, and
  nothing references the endpoint.
  Write: dogstatsd_port: "{{ self.endpoints.dogstatsd_udp.port }}"
  used by: participant 'load' connect.comparison_target
```

References in `ready` and `commands` do not wire an endpoint, and a file placed without
`template: true` is not read. A restated endpoint is still one the participant type declares, so it
must be referenced like any other.

Three things follow from "wired means referenced":

- **A literal over a wiring key unwires the endpoint.** A case that writes `DD_DOGSTATSD_PORT: "0"`
  turns the UDP listener off. Afterwards a `send` to `dogstatsd_udp` is a plan error, not a datagram
  nothing receives.
- **The case states which key carries an address.** `apm-trace` gives the trace-agent a literal
  under `DD_APM_RECEIVER_PORT` and gives ADP the endpoint under `ADP_DD_APM_RECEIVER_PORT`. The
  harness needs no model of which process owns the receiver.
- **Wired does not mean listening.** The harness does not read the switches that turn a listener on.
  A wired endpoint the software never opens fails when the case runs.

**The withdraw rule.** The Agent and ADP let environment mask `datadog.yaml`, so a default
environment value would override a key the case set in its file. Two rules withdraw defaults. Both
live in the `agent-adp` and `agent` participant types, look only at which keys the case sets, and
never read an address.

1. A default environment entry is withdrawn when the case's `datadog.yaml` sets the entry's file
   key. Each default entry carries its own file key, the host view's entries included. The lookup
   walks the YAML by key path and ignores letter case. The rule parses the file as the case wrote
   it, before any reference resolves, so a reference in a YAML file sits inside a quoted scalar. A
   file that does not parse is a plan error.
2. In a Linux container or pod, a default `DD_<K>` is withdrawn when the case's `env` sets
   `ADP_DD_<K>`, because a streamed default would mask the ADP-only value. The default is withdrawn
   for every process in the container, so another process falls back to the software's own default
   for that key. The rule reads no switch of the software. An `ADP_DD_` key on Windows or macOS is a
   plan error raised by `agent-adp`'s `spec`.

A withdrawn default leaves its endpoint unwired unless the case's own line is a reference. That is
the common path in the comparison cases, and the wired check's message covers it.

**Readiness evidence** belongs to an endpoint, and only a consumer waits for it, within 60 seconds
by default. Participant types override the protocol's default in four places. `datadog-intake` answers
HTTP 200 at `/ready` on `api`, and its `dogstatsd_forward` waits for the intake's own log line that
its DogStatsD listener is bound. The intake is a harness tool, so the line is ours to keep; `cmprest`,
the first PR with a forwarding connection, adds it with the consumer. `agent-adp`'s `dogstatsd_udp` waits for ADP's "DogStatsD listener started"
line for that port. `agent`'s `dogstatsd_udp` needs evidence that holds across Agent releases.
`compare`, which writes the `agent` participant type, gives it one: a log line the Agent prints
when the listener is bound, if one holds across the releases the cases use, and otherwise a gate
on the Agent's own health check, as legacy waits for it today.

**What is found only at run time.** The harness does not model the software, so some mistakes are
not plan errors. A `connect` to a listener the software has switched off times out at its gate, and
the error names the endpoint and the evidence. A UDP listener bound to loopback with a peer
connected is not detected: the load is lost and `compare` fails. A literal over a wiring key, by
contrast, is a plan error with the line to write.

**What migrating a case changes.** For addresses, only the lines that state an address the harness
now owns: a port literal on a wiring key, an exposed port, a hard-coded address of another
participant. No `connect` line and no step's `endpoint:` field changes. The software sees the same
keys. A port number, a socket file name or a pipe name differs wherever the harness now owns it.
That includes every comparison case, because comparison cases run on the software's default ports
today.

**The fallback.** If owning addresses cannot be built, the harness reads each address from the
case's configuration. A participant type's table then lists, for each endpoint, the keys that set
it, first one set wins, and a default. There is no wired check and no case line changes for
addresses. The costs: the harness parses software values (a port, `host:port`, `tcp://host:port`, a
path), a participant type reads file contents while planning, and key order stands in for which
process owns a listener. The withdraw rule, readiness, connections, routes and backends are the same
in both designs.

### Connections and routes

`connect: {<name>: <participant>.<endpoint>}` is written on the consumer. It does four things.

1. **Checks.** The server has the endpoint on its realization, the endpoint is usable, and a route
   exists from the consumer to the server for that protocol.
2. **Orders.** The server starts in an earlier start level than the consumer.
3. **Gates.** The consumer's start waits for the endpoint's readiness evidence. The gate fails as
   soon as the server is seen to exit.
4. **Delivers.** The address, as the consumer reaches it, becomes
   `{{ self.connections.<name>.<url, host, port or path> }}` in the consumer's configuration.

**Typed and untyped delivery.** A participant type declares the connection names it knows, as data
in its spec (`consumes`).

| Participant type | Name | Accepts | Sets |
| --- | --- | --- | --- |
| `agent-adp`, `agent` | `intake` | `http` | `DD_DD_URL` and `DD_APM_DD_URL`, both `{{ self.connections.intake.url }}` |
| `agent-adp`, `agent` | `dogstatsd_forward` (Linux containers) | `udp` | `DD_STATSD_FORWARD_HOST` and `DD_STATSD_FORWARD_PORT` |
| `millstone` | `comparison_target`, `baseline_target` (both required) | every protocol but `named_pipe` | nothing: the case's `millstone.yaml` references `{{ self.connections.comparison_target.url }}` |

A declared name checks the endpoint's protocol and adds its environment entries to the consumer. The
intake connection is optional. The entries are ordinary environment with references. Any other name,
on any participant, delivers nothing by itself: the participant's own `env`, command line or
template files must reference `{{ self.connections.<name>.<part> }}`. A connection nothing uses is
an error, which is how a misspelt name is caught. A participant with no participant type always
works this way.

A delivered key also wins over the same setting in the case's `datadog.yaml`, because environment
masks the file. A stale line there, such as a `dd_url` that names a host of the legacy network, is
dead, and a migrated case drops it.

**Routes between participants.** A route is chosen per endpoint protocol, consumer realization and
server realization. The function keeps all three arguments, and the first version fills only the
cells where both ends have the same realization.

| Protocol | Linux container to Linux container | Windows container to Windows container | Pod to pod | Host to host | Two different realizations |
| --- | --- | --- | --- | --- | --- |
| `tcp`, `udp`, `http`, `grpc` | by name | by name | by name | loopback | plan error |
| `unix_datagram`, `unix_stream` | shared directory | plan error | shared directory | shared directory | plan error |
| `named_pipe` | plan error | plan error | plan error | plan error | plan error |

- **By name:** every container of a case joins the case's network under its participant name, and
  every pod has a headless Service named after its participant. The consumer's `host` is that name.
- **Loopback:** the consumer's `host` is `127.0.0.1`.
- **Shared directory:** the socket is under the server's own directory, which every participant of
  the case sees at the same path. The plan checks the location and the length (103 bytes on macOS,
  107 elsewhere).
- **A named pipe** exists only inside its own Windows container. Steps reach it; participants do
  not.

Adding a mixed case later means new cells here, a way for a container to address the runner host,
and a host port chosen before the consumer is created. The participant type interface and the
lifecycle do not change.

**How a step reaches an endpoint.** Steps, readiness gates and operations reach an endpoint from the
runner. The way depends on what is done as well as on the protocol and the server's realization.

| What is done | Linux container | Pod | Windows container | Host process group |
| --- | --- | --- | --- | --- |
| Probe a `tcp`, `http` or `grpc` port, or a Unix socket | courier in the server | courier in the server | courier in the server (no sockets) | in the runner |
| Probe a `named_pipe` | plan error | plan error | courier in the server | plan error |
| Probe `udp` | plan error | plan error | plan error | plan error |
| Send bytes | courier in the server | plan error | courier in the server | in the runner |
| Make an `http` or `grpc` request | published port | forwarded port | courier in the server | in the runner |

- **Courier in the server.** The harness runs the courier inside the server participant through
  exec, and the courier reaches the listener by the participant's name, as a peer would. A probe
  from outside proves nothing: Docker's port proxy accepts a connection even when nothing listens
  inside.
- **Published port.** A request observes a response, so it may go through a port published on the
  runner's `127.0.0.1`. The runner's own HTTP client makes it, and that client speaks TLS, which the
  courier does not.
- **In the runner.** For host processes the courier's library runs inside the harness.
- **One send, one connection.** Each send opens one connection, or sends one datagram, and writes
  the bytes as given.

Two limits are plan errors: a send to a pod, and a request through the courier to an endpoint with
`tls`. For `agent-adp`'s `otlp_http`, the participant type sets `tls` when the case sets the
receiver's certificate and key. Until a case needs them, two more are plan errors: a request to a
`grpc` endpoint, because the only request is an HTTP `GET`, and a request to an endpoint with `tls`,
which `linuxrest` builds for `otlp-tls`. A send to an `http` or `grpc` endpoint writes its bytes over
TCP.

**Runner-host ports.** Every http and grpc endpoint of a Linux container is published on the
runner's loopback address. Docker chooses each published port and the harness reads it back after
start. No participant's configuration ever holds a runner-host port, and every request from the
runner happens after its server has started, so nothing needs the number earlier. The plan marks
such an endpoint as published, with its host port chosen at start. `airlock` gains a way to publish
with no host port and reports the mapping Docker made, about 40 lines. The fixed host-process ports
are below 49152 so that they stay clear of the ports an OS hands out. If the range Docker picks from
still overlaps them on some host, the container backend publishes from a fixed range instead and
nothing else changes.

### Planning and `panoramic plan`

Plan compile turns a loaded case and one subject OS into a `CasePlan`, or an error. It runs for
every OS in the case's `subject_os` during discovery, on any host. It reads the files the case
references and the run's replacements, and nothing else: no daemon and no network.

1. **Lay out.** Give every participant a realization. The subject gets the enclosure it requests or its
   OS's default; every other participant gets the enclosure it requests or the subject's. Two
   participants of different realizations cannot connect in this version.
2. **Build each spec:** the case's fields for this OS, the participant type's defaults, the guards,
   the merge.
3. **Shape each participant.** Check the launch shape. Derive `processes` where unset. Add the
   environment of each declared connection the case made. Apply the run's replacements to `image`
   and program references. Ask the realization backend to reject what it cannot honor.
4. **Plan addresses.** Give each endpoint its address, and check that no two endpoints of one
   participant share one. Give each connection its route and its address as the consumer sees it,
   and apply the wired check.
5. **Check and fill references.** Every reference exists and is allowed in its place. Plan-time
   values are filled in; the rest is recorded with when it becomes known.
6. **Order.** Compute create order and start order as levels, and the evidence each consumer waits
   for.
7. **Build the procedure.** For each step: lay its `os` fields over its fields, build the step,
   check everything it names, and plan the route for each endpoint it reaches from the runner.

Compile reports the errors it finds. Their order is not a contract. Each error names the case, its
file, the subject OS and the participant or step.

```rust
/// Everything one case will do on one subject OS.
pub struct CasePlan {
    pub case: CaseInfo,                       // name, source file, description, timeout
    pub subject_os: Os,
    pub scope: Scope,                         // per-case resources of each enclosure; names come from the run
    pub participants: BTreeMap<String, PlannedParticipant>,
    pub create_order: Vec<Vec<String>>,       // levels of independent participants
    pub start_order: Vec<Vec<String>>,
    pub gates: BTreeMap<String, Vec<Gate>>,   // what each consumer waits for before it starts
    pub procedure: Vec<ProcedureNode<PlannedStep>>,
}
```

**The plan holds everything the lifecycle acts on.** A `PlannedParticipant` is the merged spec,
checked and made concrete: a `Launch` that is either a container (image, command, args) or host
processes, the environment, the mounts, the endpoints and connections with their addresses and
readiness evidence, every observed process, the commands, the files to collect, and every file it
places, keyed by destination. Each file carries its bytes, whether the participant type supplies it
or the case does. The case's files are read once, while the case loads and plans: a template is
parsed, and a directory source is expanded to each of its files. Each operation is kept by name as
the value a step calls. The plan owns all of this: nothing in it points back into the loaded case,
and nothing reads a file again.

After plan compile nothing reads the loaded case or calls the participant type, except an operation
a step calls. The lifecycle hands each planned participant to its realization backend as it is.

**`panoramic plan <case directory> --subject-os <os> [--text]`** loads and plans the case exactly as
`run` does and prints the whole plan as JSON. It starts nothing and needs no daemon, so it answers
"what does this case do on Windows" on a Mac. The JSON is derived from the plan types with
`serde::Serialize`, and its field names are theirs. Every map is ordered and every field keeps its
declared order, so dumping one case twice gives identical output, and any two dumps diff line by
line. A file appears with its contents, a template as written and, where planning filled it, as
resolved, and an operation as its name and what it uses. Migration reviews read this JSON beside
the legacy file. A case that fails may also write its plan to its log directory.

This is part of the plan for `dsd-plain`, trimmed:

```text
$ panoramic plan test/correctness/cases/dsd-plain --subject-os linux
{
  "case": {
    "name": "dsd-plain",
    "file": "test/correctness/cases/dsd-plain/config.yaml",
    ...
    "timeout": "20m"
  },
  "subject_os": "linux",
  ...
  "participants": {
    "adp": {
      "participant_type": "agent-adp",
      "subject": true,
      ...
      "env": {
        "DD_DATA_PLANE_API_LISTEN_ADDRESS": {
          "written": "tcp://0.0.0.0:{{ self.endpoints.api.port }}",
          "resolved": "tcp://0.0.0.0:45100"
        },
        ...
      },
      "files": [
        {
          "destination": "/etc/services.d/data-plane/finish",
          "source": "agent-adp: s6 adp finish",
          "mode": "0755",
          "name": null,
          "contents": {
            "text": "#!/bin/sh\n\n# Records each exit of ADP for the harness, ..."
          }
        },
        ...
      ],
      ...
      "connections": {
        "intake": {
          "target": {"participant": "intake-adp", "endpoint": "api"},
          "protocol": "http",
          "route": "by_name",
          "url": "http://intake-adp:2049",
          "reached": {"host": {"host": "intake-adp", "port": 2049}},
          "delivers": ["DD_DD_URL", "DD_APM_DD_URL"]
        }
      },
      ...
      "operations": {
        "config_dump": [{"command": "adp"}],
        ...
      },
      ...
    },
    "load": {
      "participant_type": "millstone",
      ...
      "connections": {
        "baseline_target": {...},
        "comparison_target": {...}
      },
      ...
    },
    ...
  },
  ...
}
```

`--text` prints the same plan in a format for people. No test pins the content of either format:
the cases passing is the test of what they show.

```text
$ panoramic plan test/correctness/cases/dsd-plain --subject-os linux --text
case      dsd-plain            test/correctness/cases/dsd-plain/config.yaml
subject   linux                timeout 20m

adp       agent-adp            subject     linux-container
  image       saluki-images/datadog-agent:testing-devel
  command     (the image's own)
  env         DD_API_KEY=correctness-test
              DD_DATA_PLANE_API_LISTEN_ADDRESS=tcp://0.0.0.0:45100
              DD_DD_URL=http://intake-adp:2049
              ...
  files       /etc/datadog-agent/datadog.yaml     <- datadog.yaml (template)     0644  as datadog.yaml
              /etc/services.d/data-plane/finish   <- agent-adp: s6 adp finish    0755
              ...
  endpoints   api            http           45100                                  ready: accepts
              dogstatsd_uds  unix-datagram  /airlock/adp/shared/dogstatsd_uds.sock ready: socket exists
  connect     intake -> intake-adp.api   http://intake-adp:2049   delivers DD_DD_URL, DD_APM_DD_URL
  processes   adp     exit: record data-plane   output: file adp.log
              agent   exit: record agent        output: file agent.log
  commands    adp  agent
  operations  config_dump  config_set  dogstatsd_capture  dogstatsd_replay  dogstatsd_stats

(agent, intake-adp, intake-agent and load omitted here)

order
  create      adp  agent  intake-adp  intake-agent  load
  start  1    intake-adp  intake-agent
         2    adp      after intake-adp.api answers 200 on /ready
              agent    after intake-agent.api answers 200 on /ready
         3    load     after adp.dogstatsd_uds exists, agent.dogstatsd_uds exists

procedure
  1  log_contains  on load  process baseline    'Millstone finished sending'  15m
  2  log_contains  on load  process comparison  'Millstone finished sending'  15m
  3  process_stable_for  on load  32s
  4  compare       metrics  comparison intake-adp  baseline intake-agent
```

When a run replaces a reference, the plan shows both: the reference as written and the one the run
uses. In `--text` that is
`image  saluki-images/datadog-agent:testing-devel -> registry.example/agent:abc123`.

### The lifecycle

One function runs one case from its plan. The runner in the binary schedules cases and owns reclaim.

| Phase | What happens |
| --- | --- |
| prepare | Each realization backend prepares its part of the case's scope: a network and a volume, a namespace, or a state directory. Every resource is entered in the ledger before it is made. |
| create | In create order, each participant is created from its data. References whose values exist are resolved and files are placed. Nothing runs yet. |
| start | Participants start level by level. A level starts once every endpoint its participants connect to shows its readiness evidence. A gate fails as soon as its server is seen to exit. |
| procedure | The steps run in order, and a `parallel` block runs its steps together. Each step has its own time budget inside the case's timeout. |
| observe | Exited participants get a short grace to finish their output, then the files to collect are copied out. This also runs after a failed create or start, if anything started. |
| reclaim | The ledger removes everything the case made, under its own budget, on every path: pass, failure, error, timeout and cancellation. Each resource is reported as reclaimed, leaked or unsettled. |

- **Two orders.** Create order exists because one participant's data may need a value another
  participant gets when it is created, such as a container ID. Start order follows connections.
- **Only consumers wait.** Readiness is observed per participant, through the evidence of its
  endpoints. A participant nobody connects to starts and the procedure begins, so a case that
  expects startup to fail needs no special field.
- **Failed is not errored.** Every handle operation says whether a problem is the software's or the
  harness's. The first makes a step `failed`, the second makes it `errored`.
- **The primitives exist.** Deadlines, tasks, cancellation and the ledger are on `main` today
  (`probe.rs`, `ledger.rs`, `tasks.rs`) and move into `core`.

### Realization backends

A backend takes a planned participant and realizes it. It has two seams: static facts for planning,
and a run-time participant that creates, starts, observes and removes. Each backend also checks a
participant at plan time and rejects what it cannot honor. There are three backends for four
realizations, because one container backend covers Linux and Windows containers.

|  | Linux container | Windows container | Kubernetes pod | Host process group |
| --- | --- | --- | --- | --- |
| Per case | one bridge network, one volume | one `nat` network | one namespace, one directory on the kind node | one state directory under `/tmp` |
| Launch | image, with the spec's command and args or the image's own | the same | one pod with one container, and a headless Service | each program started so that it and its children can be signalled together, in the participant's directory, with a cleared environment plus `PATH`, `HOME`, `TMPDIR` |
| Files | one archive placed between create and start | the same, under `C:\` | written on the node before the pod exists, each mounted at its destination | written under the participant's directory |
| `{{ self.dir }}` | `/airlock/<p>/shared` on the case's volume, mounted in every container of the case | `C:\airlock\<p>\shared`, inside the container only | `/airlock/<p>/shared` on the node directory every pod of the case mounts | `<state directory>/<p>/shared` |
| Reserved paths | `/panoramic/courier`, `/airlock/<p>/exit`, `/airlock/<p>/log` | `C:\panoramic\courier.exe`, `C:\airlock\<p>\exit`, `C:\airlock\<p>\log` | as a Linux container | none |
| Courier | copied in with the files | copied in with the files | mounted read-only from the node | the library, inside the harness |
| Exit of a process | the container's exit, or a record file | the same | the container's terminated state, or a record file read through the node | the wait result of the child |
| Output of a process | the container log, or a log file followed by the courier | the same | the container log, or a log file read through the node | the child's pipes |
| Stop and remove | stop with a grace, force-remove; then containers by label, volume, network | the same, no volume | delete pod and Service; then the namespace and the node directory | SIGTERM each program and its children, SIGKILL after the grace; remove the state directory |

The table leaves out five points.

- **One container backend.** The Linux and Windows files implement the same traits method by method.
  Their differences become a table of constants per container OS: the shared root and separator, the
  courier path, the network driver, whether there is a case volume, whether ports publish, whether
  host mounts are allowed, the PID namespace and the options accepted.
- **The courier follows log files in containers.** A supervised process writes its output to a file.
  Following it with `sh` and `tail` from the image would make both a hidden requirement on any image
  a case names, so Linux containers use the courier's `follow`, as Windows containers do. A pod's
  log file is read through the node.
- **A pod is submitted in the start phase.** Create writes its files and its Service; start submits
  the pod and waits for it to run. A pod's command is wrapped in a shell loop that waits for late
  files only when the participant has a template file holding a value known after start. Among
  today's cases that is `millstone` in the kind cases. Such a pod's image needs a shell, and that is a
  stated requirement of the pod realization for a participant with a late-written template file.
  Every other pod runs its image unwrapped and needs no shell.
- **Host-process cases run one at a time.** The scheduler holds one extra permit that a case takes
  when its layout has a host process group. Two invocations on one host can still collide, which
  stays a host rule.
- **Not built:** pods with more than one container, and routes between different realizations.

### Observation and results

Every process a participant declares has one exit channel and one output channel.

| Process | Exit | Output |
| --- | --- | --- |
| A host process | The wait result of the spawned child | Its stdout and stderr pipes |
| The only process of a container or pod | The container's exit | The container log |
| A supervised process (s6, the Windows entrypoint) | A record file the supervisor appends to | A log file the backend follows |

- **Three states.** An exit is running, exited with a status, or undetermined. Undetermined is an
  observation error: a step that needs the status errors. It never passes and no code is invented.
- **Exit records.** Every container and pod gets `PANORAMIC_EXIT_DIR` and `PANORAMIC_LOG_DIR` in its
  environment. A supervisor appends one line per exit to `$PANORAMIC_EXIT_DIR/<name>`:
  `<code> <signal> <seconds>.<fraction>`, a Unix time with one to nine fractional digits. A normal
  exit has signal 0. A death by signal has code 256 and the signal number. The first line is final,
  so a restart never returns a process to running. A case can declare supervised processes of its
  own, so this format is a contract and the user guide documents it.
- **Signals.** A container reports death by signal as code 128 + n, while a record and a waited
  child carry the signal separately. `exits_with` asserts a code, and no case asserts on death by
  signal today. Matching a signal the same way on every realization is added with the first case
  that needs it.
- **Output.** Output is written under `<log dir>/<participant>/` as it arrives, and each line keeps
  the name of its process.
- **Reports.** `run.json` and `result.json` keep today's shape and gain fields: the subject OS, the
  skipped outcome with its reason, each participant's realization, per-process exits and output
  paths, reclaim entries and collected files. Exit codes are 0 passed, 1 failed, 2 harness error,
  and 3 when nothing ran.
- **Retention is data.** A file to collect, whether a participant's data lists it or a step writes
  it, carries `keep: always` or `keep: on_failure`. The runner applies the rule and knows no file
  names.

### Steps and operations

A step is a registered name and a struct read from the case's fields. A step never learns how a
participant is realized, and it does not know a participant type: it names a capability, and any
participant that offers it works.

```rust
//! core::step

/// A registered step: builds a `Step` from its fields.
pub trait StepFactory: Send + Sync {
    fn name(&self) -> &'static str;
    /// An assertion checks a condition; an action changes state. Only assertions may sit in `parallel`.
    fn purpose(&self) -> StepPurpose { StepPurpose::Assertion }
    /// The fields that may hold references. A reference in any other field is an error.
    fn template_fields(&self) -> &'static [&'static str] { &[] }
    /// Decodes and checks the fields. Does no I/O.
    fn build(&self, fields: &StepFields) -> Result<Box<dyn Step>, String>;
}

/// A step built for one case and one subject OS.
#[async_trait]
pub trait Step: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;
    /// Everything of the case the step uses, so the plan can check it before anything starts.
    fn needs(&self) -> Vec<Need>;
    fn budget(&self) -> Duration;
    /// The driver may drop this future on cancellation or when the budget runs out.
    async fn run(&self, cx: &StepContext<'_>) -> StepReport;
}

pub enum Need {
    Participant(String),
    Process(String, String),                       // participant, process
    Command(String, String),                       // participant, named command
    Operation(String, String),                     // participant, named operation
    Endpoint(String, String, EndpointOperation),   // participant, endpoint; probe, send or request
}
```

A step acts on a started participant through a `Handle`, and every method works the same on every
realization: `output(process)`, `exit(process)`, `wait_for_exit`, `read_file`, `exec`,
`command(name, args, env)`, `op(name, args)`, `probe(endpoint)`, `send(endpoint, payload)` and
`http_get(endpoint, path)`. A `HandleError` is either `Harness`, when the mechanism failed, or
`Software`, when the mechanism worked and the software answered badly. The split is what makes a
step `errored` and not `failed`.

Knowledge about one piece of software stays with its participant type, in two forms.

**Commands are data.** `commands` in the spec names the software's command lines for each
realization. Nothing else in the harness writes a program path of the Agent or ADP.

| `agent-adp` as | `adp` | `agent` |
| --- | --- | --- |
| Linux container | `/opt/datadog-agent/embedded/bin/agent-data-plane` | `/opt/datadog-agent/bin/agent/agent` |
| Windows container | `C:\adp\agent-data-plane.exe` | `C:\Program Files\Datadog\Datadog Agent\bin\agent.exe` |
| macOS host process group | its `adp` program, then `-c datadog.yaml` | its `agent` program with that program's environment, then `-c .` |

**Operations are capabilities.** An operation is a name, what it uses of its participant, and a Rust
function. A step asks for the name, so a second intake implementation works with `compare`
unchanged.

```rust
#[async_trait]
pub trait Operation: fmt::Debug + Send + Sync {
    fn name(&self) -> &'static str;
    /// The commands and endpoints of its own participant the operation uses. The plan checks each.
    fn uses(&self) -> &'static [Uses];      // Uses::Command(name) | Uses::Request(endpoint)
    /// Runs on one participant, through the same handle a step holds.
    async fn run(&self, on: &Handle<'_>, args: Value, limit: Duration) -> Result<Value, HandleError>;
}
```

An operation is written once, with no list per realization. It runs wherever the commands and
endpoints it uses exist, which the plan checks.

| Operation | Offered by | Uses | Called by |
| --- | --- | --- | --- |
| `dump` | `datadog-intake` | a request to `api` | `compare`, `intake_has_metric` |
| `config_dump` | `agent-adp` | command `adp` | `config_key_equals` |
| `config_set` | `agent-adp` | command `agent` | `config_set` |
| `dogstatsd_capture`, `dogstatsd_replay`, `dogstatsd_stats` | `agent-adp` | command `adp` | `dogstatsd_replay` |

The rule for choosing: a plain command line needs no Rust, so it is a `commands` entry run with
`exec`. An operation exists where the answer must be decoded or the arguments are the software's
own.

**`exec`** runs a command inside a participant that is already running. It passes when the command
exits 0 and, if `output_contains` is set, its output holds that text.

```yaml
# A script the case supplies, in the language the image provides.
- step: exec
  on: db
  command: [sh, /checks/rows.sh, "select 1 from series where metric = 'panoramic.pg.counter'"]
  env: {PGUSER: postgres, PGDATABASE: postgres}
  timeout: 120s

# The software's own command line, by name, the same on every OS.
- step: exec
  on: adp
  command: adp
  args: [debug, runtime, show-processes]
  output_contains: "Supervision tree for 'adp-root'"
  timeout: 30s
```

`command` is a list, or the name of one of the participant's `commands`. `args` is appended. `env`
is added for this command. All three may hold references.

**The steps.** Every step may carry an `os` overlay.

| Step | Purpose | Fields | Needs |
| --- | --- | --- | --- |
| `exec` | action | `on`, `command`, `args`, `env`, `output_contains`, `timeout` | the participant; the command, when named |
| `exits_with` | assertion | `on`, `process`, `code`, `timeout` | the process |
| `process_stable_for` | assertion | `on`, `process` (optional), `duration` | the participant or the process |
| `log_contains` | assertion | `on`, `process` (optional), `pattern`, `regex`, `timeout` | the participant or the process |
| `log_not_contains` | assertion | `on`, `pattern`, `regex`, `during` | the participant |
| `file_contains` | assertion | `on`, `path`, `pattern`, `timeout` | the participant |
| `port_listening` | assertion | `on`, `endpoint`, `timeout` | the endpoint, to probe |
| `send` | action | `on`, `endpoint`, `payload`, `repeat`, `timeout` | the endpoint, to send |
| `http_check` | assertion | `on`, `endpoint`, `path`, `status: {equal \| not_equal}`, `body: {equal}` (optional), `insecure_skip_verify` (optional, default false), `timeout` | the endpoint, to request |
| `wait` | action | `duration` | nothing |
| `config_key_equals` | assertion | `on`, `key`, `value`, `view`, `timeout` | operation `config_dump` |
| `config_set` | action | `on`, `key`, `value`, `timeout` | operation `config_set` |
| `dogstatsd_replay` | action | `on`, `endpoint`, `payloads`, `capture_duration`, `stats_duration`, `expected_metrics`, `timeout` | three operations; the endpoint, to send |
| `intake_has_metric` | assertion | `on`, `name`, `metric_type`, `value`, `tags`, `tags_match`, `tag_patterns`, `max_age`, `timeout` | operation `dump` |
| `compare` | assertion | `telemetry`, `comparison`, `baseline`, and the options of each telemetry | operation `dump` on both participants |

`timeout` is required wherever it is listed and has no default. `compare` has none: its budget is
what remains of the case's timeout. Where `process` is optional, leaving
it out means every process of the participant. `http_check` passes when the status matches and, if
`body` is set, the body equals that text exactly. References are allowed in `exec`'s `command`,
`args` and `env`, in `file_contains`'s `path`, and in the `pattern` of `log_contains` and
`log_not_contains`. This table and the legacy step each one replaces are the contract for each
field.

A check only one case needs is a script that case supplies, run with `exec` in a helper participant
connected to what it must reach. It is not a step.

### Images and programs

An image or a host program is a string, written where it is used. There are no roles, no manifest
and no `artifacts` subcommands.

| Who writes it | Example |
| --- | --- |
| A case, on a participant with no participant type | `image: postgres:16` |
| A case, over a participant type's default | `image: saluki-images/datadog-agent:testing-release` on both sides of a comparison case |
| A participant type, as a constant | `saluki-images/datadog-agent:testing-devel`, `saluki-images/correctness-tools:latest`, `target/release/agent-data-plane` |
| A backend, as a constant | a helper image the backend runs, if it needs one; the courier binary for each OS |

The constants are today's defaults, which are the tags and paths the `Makefile` builds, so a local
run needs no flag.

**Replacing.** `--set NAME=REFERENCE`, repeatable, or `PANORAMIC_SET` as a comma-separated list.
`NAME` is the reference exactly as written, and the run uses `REFERENCE` in its place wherever it
was written. `panoramic plan` and `run.json` show both. A name given twice, or a name no discovered
case uses, is an error before anything starts. This is how CI points cases at the images a pipeline
built. A backend's constants are not `--set` names. A helper image a backend runs comes from
`PANORAMIC_ALPINE_IMAGE`, as the legacy runner's does today, and the check before the first case
covers it; the courier binary is built by `make build-courier`.

**Before the first case**, once, after selection:

1. Every realization the selected cases need is available: a Docker daemon for containers, kind for
   pods, the right host OS for host processes. If not, that is a harness error.
2. Every image the selected cases use is in the daemon. An image the daemon lacks is pulled here,
   once, outside any case's timeout. An image that cannot be pulled is a harness error that names
   the image and the cases that use it.
3. Every host program exists and can be executed, and the courier binary for each participant OS in
   use exists.
4. For pod cases, the kind cluster exists and holds the images. Images are loaded with
   `kind load docker-image`, which needs a single-platform image. The host requirement for pod cases
   is a single-node kind cluster.

Pulling a missing image keeps "run a new container" a one-line change in a case, in CI as well as
locally. It is also what the harness does today, and the comparison jobs depend on it.

### Testing the harness

The cases are the main test. A test of the harness alone covers what a passing case cannot show.

| Suite | Shows | Lives | Runs |
| --- | --- | --- | --- |
| Bad cases | A bad case file is rejected, for a given subject OS, with a given message. A few cases, for the trickiest checks: the guards, the wired check, start order and when a reference has a value | Case files as data under `bin/correctness/panoramic/testdata/bad/`, with one driver test | Unit test jobs |
| Failing checks | Each step fails when its condition is false, and errors when the mechanism fails or an exit is undetermined | Beside each step, against one scripted handle | Unit test jobs |
| Cleanup, scheduler | Deadline, cancellation and reclaim leave nothing behind and record the outcome | With the runner, the ledger and tasks; most exist on `main` | Unit test jobs |
| Cleanup, containers | A failed start and a cancelled case leave no container, network or volume | Ignored tests beside the container backend and in `airlock` | By hand, `make test-container-daemon` |
| Cleanup, host processes | The same, for a process group and its directory | Unit tests beside the backend | Unit test jobs on Linux and macOS |
| Cleanup, Windows containers | The same | Ignored tests | By hand, on a Windows host with Docker |
| Cleanup, pods | A failed start leaves no namespace or node directory | One ignored test | By hand, `make test-container-daemon` |
| Real cases compile | Every case compiles for every OS it declares | One test over every case directory | Unit test jobs |
| The boundary | `core` names neither `participants` nor `steps`, and those two do not name each other | One rule in `test/architecture` | Unit test jobs |

- A test that needs a participant writes one in YAML, with no participant type. There are no fake
  participant types.
- No test pins the order of errors, or the text of help, listings or printed plans.
- Each suite grows in the PR that adds what it tests. No PR is a tests PR.
- The driver and the step tests are unit tests in the binary crate, because `make test` runs
  `--lib --bins` and would skip a `tests/` directory.
- Tests that need a Docker daemon or a kind cluster are ignored, so `make test` and `make test-all`
  need neither. No CI job runs them; a person changing the mechanisms they cover runs them by hand.

The PRs add about 6k lines of harness tests, beside the tests on `main` that stay.

## Proposed PR Plan

The work lands as 16 PRs in six stages. The first two are independent and touch nothing a case uses.
The next six are one stack that ends with real cases running on the new path, among them the intake
cases, which exercise harness-owned addresses end to end. After that stack, every PR is justified by
the cases it turns over.

**How old and new coexist**

- **One crate.** The new code lives in the existing `panoramic` crate as three new modules, `core`,
  `participants` and `steps`, beside the legacy modules. New modules never import legacy modules;
  legacy modules may import new ones.
- **Routing by file shape.** A case file with the legacy top-level `type:` key goes to a legacy
  runner. A file without it goes to the new loader. A case file is legacy or new for every OS it
  runs on, never both.
- **One version of each case.** A PR that migrates a case deletes the legacy file it replaces. A
  legacy and a new version of one case never coexist in the tree.
- **A new legacy case keeps running.** A legacy case a teammate adds keeps running on the legacy
  path, with no action from anyone, until the last legacy case of its directory is migrated. After
  that, a legacy file in that directory fails at load, so it never runs without the CI inputs its
  path needs. A directory here is a cases directory a CI job runs, such as `test/integration/cases`.
- **One scheduler.** The new path is a third implementation of the `Test` trait. The scheduler, the
  case deadline, cancellation, reclaim, the reports and the TUI are shared.
- **The same CI jobs.** One binary routes each file, so the job list does not change. `--runtime mac` and `--subject-os macos` select
  the same OS, and the default for both is the host's. Each path reads its own image inputs:
  `--image-override` for legacy cases and `--set` for new ones. A `--set` name no case uses is an
  error, so each pair enters a CI job in the PR that migrates the first case that uses it.
- **Copies of `main`'s data are checked.** Where the new path holds its own copy of something
  `main` owns, the copy is checked against `main`'s version after every rebase, until one of the two
  is deleted. `agent-adp`'s default environment is a copy of what
  `bin/correctness/panoramic/src/test_env.rs` injects, and its s6 scripts are copies of
  `docker/s6-services/`, the image's own. They sit beside them, in `docker/s6-services-panoramic/`,
  and no image is built with them. The s6 scripts add exit records and log capture, so they are not
  byte copies: their check fails when `main`'s versions change, and the change is ported by hand.
- **The size of the whole.** The per-PR estimates sum to about 22k new non-test lines: about 12k of
  machinery (the Docker mechanisms, the courier, observation, the backends, the step bodies), about
  6k of the model (the case schema, the participant spec, planning, addresses and the participant
  types), and the glue in the binary.

**Rules for every PR**

- **Size.** At most about 2.5k new non-test lines, as a guide. `schema` came in over it, at about
  3.4k; `wiring` came in under it. Test lines
  stay below non-test lines. Moves, deletions and case files are counted apart.
- **Which cases.** What a case needs decides the PR that migrates it. A case belongs to the first
  PR whose layer, that PR with the PRs it depends on, has the backend for every OS the case
  declares, its steps and its participant types. A case that arrives on `main` later goes to a PR by
  the same rule, among the PRs not yet merged. A PR entry's "Migrates" names the families or the
  examples the rule sends there, never a count; the cases are whatever the rule finds at the PR's
  base.
- **Commits.** A PR that does two things keeps them in separate commits: a move apart from new code,
  and one family of cases apart from the next. A reviewer can then read one at a time, and a part
  that holds the PR up can be split off.
- **Stacks.** A stack is built to its tip, and the tip is green on a draft PR, before its first PR
  is marked ready.
- **Named changes.** A change to what a case checks, or to the environment it runs in, is named in
  the PR's first line. A case that turns red under stricter observation is a finding for a person,
  not a reason to bend the case.
- **Last case out.** The PR that migrates a directory's last legacy case removes that directory's
  legacy CI inputs, because the legacy path rejects an override no case uses. The same PR adds the
  check that a legacy file in that directory fails at load.
- **Checks.** Before review, `make fmt`, `make check-clippy`, `make check-docs`, and `cargo nextest`
  on the tests the change touches. CI runs the rest.

**What a PR that migrates cases shows**

1. The inventory from `panoramic list --json`, one row per case and OS, for each directory, at the
   base and at the head. The diff is empty, or every line of it is named.
2. The JSON `panoramic plan` prints for each migrated case and each OS it declares, read beside the
   legacy file: the image, the command, the environment, each file's destination and contents, and
   the addresses.
3. For each family whose addresses change, the configuration the software reports on both paths. The
   only differences are addresses the harness now owns.
4. The same jobs, green.
5. One check broken on purpose, with its failing output pasted and not committed.

**Overview**

Sizes are new non-test lines and new test lines, and are estimates; moves and deletions are counted
apart. The last column names what the migration rule sends to each PR.

| # | Slug | Title | Depends on | Size | Cases migrated |
| --- | --- | --- | --- | --- | --- |
| 1 | `airlock` | airlock: container mechanisms for the new backends | none | 1.5k / 0.9k | none |
| 2 | `courier` | Add the courier, an in-container probe and send tool | none | 1.1k / 0.5k | none |
| 3 | `schema` | The core module, the participant case schema and the plan command | none | 3.4k / 0.5k as built, and moves | none |
| 4 | `wiring` | Harness-owned addresses and the first two participant types | 3 | 2.2k / 0.1k as built | none |
| 5 | `container` | Container backend, courier routes and handles | 1, 2, 4 | 1.8k / 0.5k | none |
| 6 | `lifecycle` | Observation and the one lifecycle | 5 | 1.8k / 0.6k | none |
| 7 | `firstrun` | Run participant cases; migrate the first Linux cases | 6 | 1.9k / 0.6k | Linux-only cases that check logs, files and stability |
| 8 | `intake` | Intake steps; migrate the intake cases | 7 | 1.2k / 0.4k, and moves | Linux-only cases that send to an endpoint or check the intake |
| 9 | `compare` | compare, agent and millstone; first DogStatsD cases | 8 | 1.0k / 0.3k, and moves | DogStatsD comparison cases that send metrics over a datagram socket |
| 10 | `cmprest` | Migrate the remaining comparison cases | 9 | 0.9k / 0.25k | The other DogStatsD comparison cases, OTLP metrics, traces and logs; disabled cases with their family |
| 11 | `pod` | Pod backend; migrate the origin detection matrix | 9 | 1.6k / 0.3k | Cases that run in pods: the origin detection matrix |
| 12 | `linuxrest` | Probe, exec and config steps; finish the Linux-only cases | 8 | 1.6k / 0.6k | Linux-only cases that probe, request, exec or check configuration |
| 13 | `hostproc` | Host process backend; migrate macOS and Unix cases | 12 | 1.1k / 0.5k | Cases on macOS, alone or with Linux |
| 14 | `windows` | Windows containers; migrate the Windows and three-OS cases | 12, 13 | 1.0k / 0.4k | Cases on Windows, alone or with Linux; cases on all three OSes |
| 15 | `rmlegacy` | Delete the legacy runners; rewrite the testing guide | all above | deletions, docs | none |
| 16 | `followups` | The os overlay, the intake on macOS and Windows, a case-supplied helper | 15 | 0.55k / 0.1k | Cases that differ only by OS merge; the intake cases gain macOS and Windows; one new case |

The stages are foundations (1–2), the Linux slice (3–8), comparison cases (9–11), Linux-only cases
(12), other platforms and the deletion (13–15), and after the migration (16). The comparison stage
needs only the slice, and so does `linuxrest`, so two people can work them side by side. The Windows
job is the slowest CI loop, so `windows` should start early as a draft.

Why this order:

- **Addresses first.** Harness-owned addresses are the riskiest decision. `wiring` builds them
  before any backend exists and writes `agent-adp`'s whole endpoint table, and `intake` proves them
  on real cases. `compare` comes straight after the slice, because it is where two programs in one
  container share listener keys, and it needs no new backend.
- **The decision is confirmed before its code merges.** `wiring` is not marked ready until a draft
  of `compare` and `cmprest`, cut down to `dsd-plain`, `dsd-udp`, `apm-trace` and one OTLP metrics
  case, runs them green on top of the slice. If it cannot, the fallback is taken then, with nothing
  to undo on `main`. That draft is the one place the decision is taken.
- **Comparison before platforms.** It is also where teammates most often add cases.
- **One deletion, at the end.** A migration PR can be reverted while the legacy code is there.
  `rmlegacy` is the one point of no return. It is ready when no legacy case file remains, and then
  it waits for a week of green nightly pipelines. From `pod` until then the comparison runner stays
  in the tree with no case to run.
- **Nothing ships unused.** The `os:` overlay, the general participant on a host and in a Windows
  container, and the intake on macOS and Windows each land with the first case that uses them. Those
  cases change names or add coverage, so they come after the migration, together in `followups`.

### Foundations

**1. `airlock`: container mechanisms for the new backends**

- **Builds:** the Docker layer the container backend stands on. A network and shared volume prepared
  before any container, named containers, exec with standard input and an exit code, placing and
  reading files, log and exit streams, stop and removal bounded by a deadline, and a published port
  that Docker chooses and airlock reports. Adds `make test-container-daemon`, which runs the ignored
  daemon tests by hand. Legacy behaviour does not change.
- **Watch out:** `driver.rs` is shared with the legacy path. The new files call `Driver` methods
  `main` does not have; add them beside the legacy half, which stays. CI cannot reach Docker Hub;
  the test image comes from the internal registry. Keep the tests to exit codes, failures and
  removal. Resource names must not collide with the legacy path's when both run in one invocation.
  One daemon test settles whether Docker reports the port it chose through `bollard`.

**2. `courier`: the in-container probe and send tool**

- **Builds:** the crate `panoramic-courier`, a library and a static binary with four operations:
  `probe`, `send`, `http` and `follow`. `build-panoramic-binary` builds and publishes the binary.
- **Watch out:** do not set `rust-lld` for every musl target in `.cargo/config.toml`. Scope the
  linker to the courier's make target. The vocabulary is frozen at four operations. Test only
  refusals: a closed port, a timeout, bad arguments.

### The Linux slice

**3. `schema`: the core module, the case schema, specs and the plan command**

- **Builds:** two things, in this order.
  - The `core` module from what exists. Moves `ledger`, `tasks`, `probe`, `deadline`, `record` and
    the result structs of `reporter.rs` under `bin/correctness/panoramic/src/core/`. Adds one rule
    to `test/architecture`: `core` names neither `participants` nor `steps`, and those two do not
    name each other.
  - The foundation of the design: a participant is a spec, and a participant type is one function
    that fills it in. Strict loading of the one case schema, `ParticipantSpec`, the
    `ParticipantType` trait and registry, the participant with no participant type, the case's
    fields laid over a spec with the guards, a realization per participant, and
    `panoramic plan`, which prints the plan as JSON derived from the plan types, or for people with
    `--text`. Starts the bad-case suite: a directory of case files and one driver.
- **Watch out:** this PR runs over the size cap, so `--set` moved to `firstrun`. The moves are their
  own commits, with imports only and no behaviour change.
  - These tie the moved files to legacy code and must not follow them into `core`:
    `record.rs` imports a legacy step struct, which moves too; `TestResult::from_assertions` takes a
    legacy struct and stays behind; `ledger.rs` names airlock's legacy `Driver`, which stays until
    `rmlegacy`.
  - The symbol scan in `test/architecture` matches every path segment, and `participants` and
    `steps` are ordinary variable names, so the rule must match the module path or `use` items only.
    A second, temporary rule keeps legacy module names out of the three new modules until
    `rmlegacy`.
  - No participant type is registered yet, so the examples and the bad cases are participants with
    no participant type, in a container. The guards therefore have no failing input until `wiring`,
    which adds their bad cases.
  - The `Step`, `StepFactory`, `Need` and `Operation` traits land here as signatures, and `Handle`
    as a declaration, because the spec and the loader name them.
  - `os` is an unknown key until `followups`. The entry and spec field lists, and the test that they
    agree, land here.
  - Error order is not a contract and no test pins it. Build specs through constructors that cannot
    express an invalid process.
  - `make check-docs` lints doc comments, and `{{` outside fenced code breaks the docs build.

**4. `wiring`: addresses, connections, references and the first two participant types**

- **Builds:** the addresses decision, the rest of plan compile, and the first participant types that
  use them.
  - An endpoint is a name, a protocol and an address the harness owns. One resolver handles endpoint
    references and connection references. `connect` delivers an address by reference. The wired
    check, and the check that no two endpoints of one participant share an address. Start order
    and readiness evidence from connections. The planning half of the backend
    seam for Linux containers. `plan` now prints every address and connection.
  - `agent-adp` and `datadog-intake` for Linux containers, as definitions and nothing else: the
    endpoint table, the default environment that references it, the two withdraw rules, the file
    names, the processes, the s6 overlay, and `with: {host_view: true}`. `agent-adp`'s
    `with: {own_ip: ...}` lands with `windows` and its `dogstatsd_forward` consumer with `cmprest`,
    each with its first case. Teaches the intake binary to read its two listen variables and
    rebuilds the tools image. Adds the bad cases for the guards and the wired check.
- **Watch out:** this is the PR to get right.
  - Whether to keep harness-owned addresses or take the fallback is decided on a draft of the
    comparison PRs, before this PR is marked ready (see "Why this order").
  - A UDP probe is a plan error, and a UDP listener's evidence is its log line.
  - Write `agent-adp`'s whole endpoint table here, not a slice of it. An endpoint added later would
    turn the literals of cases migrated earlier into plan errors. The always-on listeners start from
    the values in `bin/correctness/panoramic/src/test_env.rs` on `main`, which are below 49152.
  - The default environment and the s6 overlay are copies of `main`'s, so from here on each is
    checked against `main` after every rebase (see "How old and new coexist").
  - The withdraw rules must match what the Agent and ADP do. Add the subjects of the hardest
    comparison cases as must-compile data: `apm-trace` (two keys for one port, two programs),
    `dsd-udp` (a file key that withdraws the UDP default and turns the sockets off) and one OTLP
    metrics case.
  - `commands` and `operations` are added by the PRs that first use them. The s6 assets need the
    `.gitattributes` line-ending rule.

**5. `container`: the container backend and handles**

- **Builds:** the run-time half for containers: one container backend with its Linux table, the
  courier client and the route for each operation, published ports read back after start, and the
  handle a step holds.
- **Watch out:** nothing calls this until `firstrun`, and the crate denies warnings. Put
  `allow(dead_code)` on the new modules and remove it in `firstrun`. Keep per-OS differences as data
  from the start, though the Windows table waits for `windows`. Linux containers follow log files
  with the courier, not `sh` and `tail`. The tests here are cleanup after a failed start and after
  cancellation, against a real daemon, run by hand.

**6. `lifecycle`: observation and the one lifecycle**

- **Builds:** exit observation with its three states, exit records, output capture, collection with
  retention as data, and the lifecycle: prepare, create in order, start behind readiness gates,
  steps, observe. Reclaim stays with the scheduler.
- **Watch out:** the lifecycle reads only the plan. It does not call the participant type at create
  time or read the loaded case; only an operation a step calls runs participant type code. An
  undetermined exit is an error, never a pass. Only a consumer waits for readiness.

**7. `firstrun`: run participant cases**

- **Builds:** the route from a case file to a run. Discovery sends a file with no top-level `type:`
  to the new loader. A third `Test` runs a plan. `--subject-os`, a recorded skip, an unsatisfiable
  case as a harness error, the checks before the first case, and `--set` with `PANORAMIC_SET`
  (moved here from `schema`). The step driver with
  `log_contains`, `log_not_contains`, `process_stable_for`, `file_contains` and `parallel`. The test
  that every real case compiles for every OS it declares. Migrates the Linux-only cases on
  `agent-adp` that need no other step, such as `adp-ipc-no-dns`, `adp-logging-default-path` and
  `adp-rar-disabled`.
- **Watch out:** the scheduler in `bin/correctness/panoramic/src/runner.rs` is shared; do not fork
  it.
  - The third `Test` answers what shared code reads. `images()` returns none, because `--set` checks
    its references. `runtime()` never says kind until `pod`. `suite()` is the suite of the case's
    directory, so log and artifact paths do not move. Traffic retention is skipped, because the new
    path's retention is data.
  - Shared code has gaps: there is no skipped outcome, and `--fail-fast` stops on anything that
    did not pass, so a skip must count as not failed.
  - Two named changes start here and hold for every Linux case after: `process_stable_for` watches a
    process where legacy watched the container, and a container no longer gets `/proc`, the cgroup
    tree and the Docker socket unless the case asks for the host view.
  - `adp-rar-disabled` must say which process it watches: legacy watched the container, and ADP
    restarts under s6 by design there.
  - `adp-ipc-no-dns` places a hook at a path with mode `"0755"`. Legacy relied on the source file's
    executable bit.

**8. `intake`: the intake cases**

- **Builds:** what proves harness-owned addresses on real cases: operations, with the intake's
  `dump` as the first; `send` to a named endpoint through the courier; and `intake_has_metric`.
  Migrates the Linux-only cases that send to an endpoint or check the intake, such as
  `dogstatsd-intake-metric` and `dogstatsd-host-tag`. Each loses its port literals, its
  `exposed_ports` and its hard-coded intake URL.
- **Watch out:** this is the tip of the slice. Move the legacy intake's Rust definitions and the
  metric matcher under `steps` here, not a copy, and edit the legacy imports in this PR: the
  comparison analyzers and `traffic.rs` use them. The `dump` operation lives with `datadog-intake`
  in `participants`, returns what the intake answered through the handle, and names nothing in
  `steps`. These cases stay Linux-only; their macOS and Windows rows come in `followups`.

### Comparison cases

**9. `compare`: comparison cases begin**

- **Builds:** the `compare` step over two participants that offer `dump`, the `wait` step, the
  `agent` and `millstone` participant types, Unix sockets shared between participants, and the first
  cases that ask for the host view. Moves the analyzers from
  `bin/correctness/panoramic/src/correctness/analysis/` under `steps`. Adds `PANORAMIC_SET` to the
  CI mixins and both row shapes to the job generator. Migrates the DogStatsD comparison cases that
  send metrics over a datagram socket, such as `dsd-plain`.
- **Watch out:** this is the first PR where addresses meet two programs in one container and a
  baseline Agent.
  - Comparison cases run on the software's default ports today. Under one endpoint table they move
    to the harness's, so ports and socket names change in every comparison case. The configuration
    diff is the proof.
  - The comparison side of every comparison case gains `agent-adp`'s default environment keys,
    which legacy sets only for integration cases, and the baseline `agent` gets the same keys.
    Name the change and show it in the configuration diff. Both sides of a comparison must stay
    configured alike, so `agent` and agent-adp share one default environment.
  - The cases here that turn origin detection on need the host view, such as `dsd-tag-filterlist`
    and `dsd-metric-filterlist-prefix`.
  - The baseline `agent`'s `dogstatsd_udp` gets readiness evidence here (see "Readiness
    evidence"), because legacy's wait for every container's health check goes away and a load
    must not start before the Agent listens. Prefer a log line that holds across the Agent
    releases the cases use; if none does, gate on the Agent's own health check.
  - Each case keeps `image: saluki-images/datadog-agent:testing-release` on both sides. The debug
    image is slow enough to fake differences.
  - Legacy fails a case when the load generator exits during the flush wait. Write the flush as
    `process_stable_for` on the load, not as `wait`.
  - A delivered key masks stale lines in the shared `datadog.yaml` files, such as `dd_url`. Drop
    those lines as each file is migrated.
  - What a failed comparison leaves on disk must not shrink: the input capture and both intake
    dumps. They become collect entries. Say what happens to the merged traffic manifest.
  - The `millstone` crate's own test parses every `millstone*.yaml` under `test/` and fails on a
    templated `target:` line, such as `{{ self.connections.comparison_target.url }}`. It changes in
    this PR.
  - An upstream PR that adds a file under the old analysis directory after this merges lands
    uncompiled, with no conflict. Watch open PRs.
  - If one flaky case holds this PR, cut it to the few cases that its code needs to pass and follow
    with the rest as case files only.

**10. `cmprest`: the remaining comparison cases**

- **Builds:** what the remaining families of comparison cases need, each with its cases.
  - DogStatsD: `compare` of `events` and `service_checks`, UDP and stream targets, and the
    forwarding connection with `agent-adp`'s `dogstatsd_forward` consumer. Migrates the DogStatsD
    cases that send events or service checks, or send over UDP, a stream socket or the forwarding
    connection.
  - OTLP metrics: the OTLP endpoints on the baseline `agent`, and the references that replace the
    literals on both sides. Migrates the OTLP metrics cases.
  - Traces and logs: `compare` of `traces` and `logs`, the APM endpoints over TCP and a Unix
    socket, and the embedded collector's receiver endpoint on `agent` for the OTTL cases. Migrates
    the trace and log cases.
  - The new schema has no `disabled` key, so a disabled case migrates with its family and moves to
    `test/correctness/disabled`, which no job runs.
- **Watch out:** most of this PR is case files. Keep one commit per family, so a family with a flaky
  case can be split off and the others can merge.
  - DogStatsD: `dsd-origin-detection` keeps the container environment it tests today. `dsd-udp`,
    `dsd-forwarding` and `dsd-forwarding-burst` start their load on a log line. A UDP listener bound
    to loopback also prints that line, and then the load is lost and only `compare` fails. Legacy
    waits for every comparison container to be healthy before the load starts, and that wait is
    gone. The readiness evidence `compare` gives the baseline Agent's UDP endpoint replaces it.
  - OTLP metrics: these cases send to port 54317 or 4317 today. A literal left on a key that wires
    an endpoint must be a plan error, not traffic to the wrong port.
  - Traces and logs: `otlp-fixtures` regenerates payloads into the log cases' directories and a CI
    job diffs them, so keep the paths or change the generator here. `apm-trace` sets one port for
    the trace-agent and another key for ADP; it is the case the wiring rule was shaped around.
    `apm-trace-uds` needs the host view. Some trace cases name a `datadog_intake.config_path` file
    that does not exist and that legacy ignores. The strict loader rejects it, so the migration
    drops it and says so. Consider delivering the logs intake keys through the `intake` connection
    here, so the log cases stop wiring them by hand.

**11. `pod`: the pod backend and the matrix**

- **Builds:** the pod backend, kind setup driven by plans, a case's choice of realization with its
  options, and files written after start for values a pod only has then. Migrates the cases that
  run in pods: the origin detection matrix, which becomes one directory per variant, sharing files.
  Job names do not change.
- **Watch out:** once `cmprest` has merged, this PR migrates the last legacy comparison case, so it
  removes `PANORAMIC_IMAGE_OVERRIDES` from the mixins and adds the check that a legacy file in
  `test/correctness/cases` fails at load. If this PR merges first, `cmprest` does both.
  - The comparison runner stays in the tree until `rmlegacy`, with no case to run. The check keeps a
    legacy comparison file that a teammate adds after this PR from running without its image inputs.
  - Every legacy kind case migrates here, so kind setup moves from the `Test` trait to plans in one
    step.
  - `kind load` needs a single-platform image. No symlinks: Windows checkouts break them.
  - The pod is submitted in the start phase, and the start gate applies only to a pod with a file
    written after start.
  - In a pod the load generator's senders exit when they finish and never print the line the
    container cases wait on. Wait on `millstone stopped.`, or bring `exits_with` forward from
    `hostproc`.
  - The shared `millstone.yaml` has comments that contain `{{`, which a template file must reword.
  - Adds the one pod cleanup test.

### Linux-only cases

**12. `linuxrest`: probes, exec, configuration checks and case-supplied scripts**

- **Builds:** what the remaining Linux-only cases need, in two parts.
  - `port_listening`, `http_check`, `exec` of a command the case writes with `args` and `env`, and
    `config_key_equals` with the operation it needs. Migrates the Linux-only cases that need them,
    such as `adp-rc-core-disabled`, `otlp-tls`, `dogstatsd-top` and `dogstatsd-remote-commands`.
  - What `liveness-container-tags` and `dogstatsd-forwarding` need, and the host cgroup namespace as
    a Linux container option. In both cases the container runs a Python script the case supplies,
    which then starts the Agent itself.
- **Watch out:** the first part makes named changes, and the second part has a choice that decides
  its size.
  - The UDP probes in `dogstatsd-top` and `dogstatsd-remote-commands` become log lines, as every UDP
    probe does in the PR that migrates its case.
  - `otlp-tls` serves TLS on its endpoint. The cases here that set ADP's OTLP receivers to literal
    ports today write references instead.
  - There are two ways to port each script case. The first is as today: the script stays the
    command of the `agent-adp` participant. It needs only that a case may replace a participant
    type's command, with the guard that it restates its processes: the s6 records if the script
    still starts s6, or the one container process if it does not. Coverage is the script's,
    unchanged.
  - The second is as a helper beside `agent-adp`, a participant with no participant type. That is
    the first real case to run one, and it needs create order beside start order, a reference to
    another participant's container ID, and an image that has Python. The tools image on `main` has
    none.
  - The scripts hold ports and socket paths as literals. Keep the addresses they expect, or pass
    them in by reference.
  - Build only what the chosen port uses. Neither port gets a participant type or a step of its own.
    `dogstatsd-forwarding` loses a UDP probe.

### Other platforms

**13. `hostproc`: the host process backend**

- **Builds:** the host process backend, `agent-adp` as two programs on macOS, host programs as
  references that `--set` replaces, `exits_with` and `dogstatsd_replay`, and the rule that
  host-process cases run one at a time. Migrates the cases that run on macOS, alone or with Linux.
- **Watch out:** addresses are fixed, so two host cases cannot overlap. The harness enforces it and
  does not rely on `-p 1`. Legacy waits for the Agent's token and certificate before it starts
  ADP. Here both programs start together and ADP's IPC client retries, which must
  be verified on the macOS runners. Those runners persist between jobs, so the job's sweep of stale
  processes stays. Cleanup tests here are unit tests and must not bind the fixed ports.
  `dogstatsd-skip-failed-listener` loses a UDP probe.

**14. `windows`: Windows containers and the three-OS cases**

- **Builds:** the last backend table, then the cases that needed every backend.
  - The container backend's Windows table, `agent-adp` on Windows with its entrypoint script, named
    pipe endpoints, and a participant's own address in its configuration and its hosts file
    (`agent-adp`'s `with: {own_ip: ...}`). The Windows CI script builds the Windows courier. Migrates
    the cases that run on Windows, alone or with Linux.
  - The cases that run on all three OSes. Those that read logs and exits need almost no Rust. Those
    that use endpoints and commands need `config_set` and `exec` of a command the participant type
    names, which replaces the legacy CLI actions.
- **Watch out:** this job is the slowest CI loop. Start the first part early as a draft; it does not
  need `hostproc`. The three-OS cases are their own commits and wait for `hostproc`.
  - The cases shared with Linux bind to the container's own address, some through a custom
    hostname, and both mechanisms are needed on Linux too.
  - The CI image bakes in the legacy entrypoint script; a spec's own command must override it, and
    the baked one stays until `rmlegacy`.
  - Network creation collides under load on Windows; the legacy path retries it and the new
    mechanisms must too. Some lints fire only on Windows.
  - This PR migrates the last legacy integration case, so the Windows script drops
    `--image-override`, and the PR adds the check that a legacy file in `test/integration/cases`
    fails at load.
  - `dogstatsd-enabled` and `dogstatsd-autoscale-udp` each lose a UDP probe. `exec` quotes a
    command's output on failure, which the legacy CLI actions did not.
  - Adding an exit-code check to `adp-no-pipelines-exit` is new coverage; add it here only if it is
    named.
  - One red Windows row holds every case in the PR. If the three-OS cases stall, split them off as a
    PR of case files.

**15. `rmlegacy`: delete the legacy runners and rewrite the testing guide**

- **Builds:** no new code.
  - Deletes the comparison runner under `bin/correctness/panoramic/src/correctness/`, the
    comparison schemas, the generator's legacy branch and the baseline mixin.
  - Deletes the integration runners, the legacy schema, actions and assertions, `traffic.rs`, the
    routing, `--runtime` and the other legacy flags, `image_override.rs`, airlock's legacy behaviour
    and the temporary guard rule.
  - Rewrites `docs/development/testing.md` and `test/integration/README.md` as a user guide of about
    400 lines. It documents the exit record format, which is a contract for case authors.
- **Watch out:** deletion and the guide only. It is ready when no legacy case file remains, and it
  waits for a week of green nightly pipelines. After it, a top-level `type:` key is an unknown
  field.

### After the migration

**16. `followups`: the `os:` overlay, the intake on macOS and Windows, and a case-supplied helper**

- **Builds:** three things kept out of the migration because they change case names or add coverage.
  Each lands with its first case, in its own commit.
  - The one `os:` overlay on participants and steps. Merges the cases that exist once per OS only
    because a command or a path differs by OS, such as the `dogstatsd-top`, `dogstatsd-replay` and
    `adp-logging` families.
  - `datadog-intake` as host processes and in a Windows container. The intake cases, such as
    `dogstatsd-host-tag` and `dogstatsd-intake-metric`, gain macOS and Windows rows. This is the
    first added coverage.
  - One small case that runs a script it supplies, as its own participant and as a step, on Linux,
    macOS and Windows. It is the first case to write `programs`, and the first participant with no
    participant type in a Windows container.
  - Deletes this plan, after anything of lasting value has moved into `docs/development/`. This is
    the effort's final pull request.
- **Watch out:** case names change, so the inventory diff is not empty. Every row that disappears
  must reappear under the surviving name.
  - One merge rule serves participants and steps. `dogstatsd-top-windows` runs its PowerShell inline
    today, and merging moves it to a file the case supplies.
  - The intake needs a Windows tools image and a host build of the intake in CI.
  - On macOS the helper script's interpreter is a host requirement. Keep that case small; it tests
    the harness's open ends, not ADP.

Left for later, each when a case needs it: assigned addresses for parallel host-process cases, and a
host-process subject with containerized helpers.

### Risks to the plan

- **The slice is most of the new code.** About 14k of 22k new non-test lines land in PRs 1 to 8, and
  six of those migrate no case. Reviewers accept them on the strength of the stack's tip and of
  `panoramic plan` output.
- **Two PRs carry many cases.** `cmprest` and `windows` each migrate several families. One flaky
  case holds the whole PR, which is why each keeps its families in separate commits.
- **Coexistence has never run.** Each PR needs its own CI loop, and the CI loop is the long pole.
- **Main moves, and teammates add legacy cases.** A file added under a directory this plan moved
  merges without conflict and is never compiled. A legacy case added under a cases directory keeps
  running until the directory's last legacy case is migrated, and the migration rule sends it to a
  PR.
- **Sizes are estimates.** Rows marked "as built" are measured; treat the rest as plus or minus a
  third.
- **Windows and kind are fragile in CI.** Problems there show only in CI.

## Named changes and unverified points

**Changes to what cases check or run in.** Each needs an owner's yes in the PR that makes it:

- `process_stable_for` watches a process, not the container. `adp-rar-disabled` is where it bites.
- Every UDP probe becomes a log line, because a UDP probe proves nothing.
- The s6 overlay merges stderr into stdout in test containers.
- Ports and socket names change in every comparison case, and both sides gain the default
  environment keys that integration cases have today.
- Comparison loads no longer wait for every container's health check.
- A Linux container no longer gets `/proc`, the cgroup tree and the Docker socket unless the case
  asks for the host view.
- A Linux container no longer shares the runner's PID namespace. Legacy gave every one the host's,
  for origin detection; a case that needs it gets an option with that case.
- `exec` quotes a command's output on failure, which the legacy CLI actions did not.
- A load error exits 2 with no `run.json`, and a run where every case is skipped exits 3.
- The merged traffic manifest may go, once retention is data.
