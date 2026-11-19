# Working on panoramic

This page is for a change to panoramic itself. To write or run a case, read [Panoramic cases](./panoramic.md).

::: v-pre

## Modules

Panoramic is one crate, `bin/correctness/panoramic`, and one small helper crate, `panoramic-courier`.

| Part | Owns | Depends on |
| --- | --- | --- |
| The binary (`main.rs`, `cli.rs`, `runner.rs`) | The command line, case selection, the checks before a run, kind setup, scheduling and reports. It registers the participant types and the steps. | `core`, `participants`, `steps` |
| `core` | Case loading, planning, references, the lifecycle, the realization backends, handles and observation. | `airlock`, `panoramic-courier` |
| `participants` | Participant types: what each piece of software runs, and how. | `core` |
| `steps` | Assertions and actions, `compare` included. | `core`, `stele` |
| `panoramic-courier` | Probes, sends, HTTP requests and file follows from inside a container or pod. | none |

The boundary rule: `core` names neither `participants` nor `steps`, and those two do not name each other. They meet
through the traits in `core` and the registry that the binary fills. The rules are in `test/architecture`. A step
that needs something from a participant asks for an operation by name.

The courier is a program of its own. The harness copies it into each container and pod, and runs it there, because a
probe from outside cannot tell whether anything listens. `make build-courier` builds it.

## A participant type

`ParticipantSpec` (`core/spec.rs`) is a participant as data. It is the same struct for every realization. A
participant type returns it, and the participant of a case decodes into the same fields.

A participant type implements `ParticipantType` (`core/participant.rs`):

- `name` is what a case writes as `type`.
- `supports` lists the pairs of a participant OS and an enclosure that the type has a spec for.
- `spec` fills in a `ParticipantSpec` for one participant on one realization. Planning calls it once per case and
  declared subject OS. It does no I/O. The result holds defaults: core lays the fields of the case over it.

`SpecContext` is what `spec` may read: the realization and its options, the fields that the case wrote for this OS
with their references unresolved, the `connect` map, `with`, and the files of the case. `cx.with()` decodes `with`
strictly into the struct of the type.

| A participant type may | It may not |
| --- | --- |
| Read the `env` keys and literal values of the case, to choose defaults | Read a resolved address, directory or identity: none exists yet |
| Read the text of a file that the case supplies | Change what the case wrote: the fields of the case lie over its result |
| Reject what it cannot honor on this realization | Read another participant |
| Return references where a case could write one | Do I/O |

A spec carries four fields that only a type sets:

- `aliases` are the file names that a case may write as `files` keys, each with one destination.
- `consumes` are the connection names that the type knows. Each is a `Consumed`: the protocols it accepts, whether
  it is required, and the environment it delivers, as template text.
- `operations` are the capabilities that steps call by name.
- `wiring` says, for each endpoint, why a default was withdrawn and which line a case writes to wire it.

A `Consumed` is per realization. `agent-adp` consumes `intake` and `dogstatsd_forward` in a Linux container or pod,
and `intake` with `DD_DD_URL` and `DD_APM_DD_URL` only on macOS and in a Windows container, where ADP and the Agent
share one environment. A file that a type places can set `supervises`, and the guards read it.

No participant type code runs after planning, except an operation that a step calls. The plan holds everything that
the lifecycle acts on.

## Merging and guards

A participant has three layers: the spec of its type, the fields of the case, and the `os` fields of the case for the
subject OS. A step has two: its fields, and its `os` fields. One rule joins them.

- A field that is a map merges by key. An entry of the upper layer replaces the entry of the lower layer with the same
  key, whole. `env`, `files`, `endpoints`, `ready`, `commands` and `with` are maps.
- Any other field replaces the field below it, whole.
- A layer that sets `command` and not `args` removes the `args` below, as Kubernetes does.

`ParticipantSpec::lay` is the rule. Building one participant for one subject OS takes five steps:

1. Lay the overlay for the subject OS over the fields of the case. This is the case layer. `with` is not in the spec:
   the `with` of the case and the `with` of the overlay merge by key here, before the type reads them.
2. Call `spec` of the type, which may read the case layer. A participant with no type starts from an empty spec.
3. Rewrite each name in the `files` of the case layer to its destination. A Windows destination is normalized to `\`
   first, so two spellings of one path are one key.
4. Check the guards.
5. Lay the case layer over the spec of the type.

The overlay is laid first, so the guards cover the fields of an overlay with no code of their own. Do not lay it later.

A step builds from `StepEntry::fields_for(os)`. `StepFields::lay` merges a field whose value is a map on both sides
by key, one level, and replaces any other field. It reads "map" from the value and not from a list of fields, so a
field that a step adds later follows the rule. Only `env` of `exec` is a map today. `payloads` is a list and replaces
whole.

The guards stop two replacements that would fail without a message:

- **Supervision.** A spec is supervised when one of its processes reports through a record file or an output file,
  because something in the container writes those. A case that sets `command` or `args` on such a participant, or
  places a file where the type has a file that supervises, must set `processes`. Without it no exit record is written,
  and `process_stable_for` could pass on a process that never started.
- **Delivered keys.** A case may not set an environment key that one of its connections delivers, in `env`, in the
  `env` of a program, or in the `env` of a command.

Two checks apply to every merged spec. An `env` key with the prefix `PANORAMIC_`, in any letter case, is reserved.
No file may land on a path that the realization reserves.

Serde cannot combine `deny_unknown_fields` with `flatten`. The participant entry, the overlay and the spec each list
the shared fields. A test in `core/case.rs` checks that the entry has the placement fields, `os` and the fields of the
spec, and that the overlay has `with` and the fields of the spec. Add a new field to all three.

## References and when a value exists

A reference is a name for a value that the harness owns. Planning, creation and a step resolve it with one resolver,
each time with more values known. The plan records for each template when its last reference becomes known.

| Value | Linux container | Windows container | Pod | Host process group |
| --- | --- | --- | --- | --- |
| `endpoints`, `connections`, `dir` | plan | plan | plan | plan |
| `ip` | after start | after start | after start | plan: `127.0.0.1` |
| `container_id` | after create | after create | after start | none |
| `pod_uid` | none | none | after start | none |

| Place | Fixed |
| --- | --- |
| The address of a socket endpoint | plan. It may refer only to `self.dir`. |
| `command`, `args`, `programs`, `env`, a pattern in `ready` | when the participant is created |
| A template file in a container or as host processes | when the participant is created |
| A template file in a pod | when the pod is created, or after it starts if the file holds a value of the pod itself |
| `commands`, and the template fields of a step | when the step runs |
| `image`, the `path` of a program, a `files` key, `with`, `mounts`, `collect`, `processes` | never hold references |

The rule: a place accepts a reference when the value exists by the time the place is fixed. Anything else is a plan
error. Three consequences follow.

- The IP of a container cannot go in its own environment. A type that needs it, such as `agent-adp` with `own_ip`,
  writes it from inside the container.
- A pod file with an after-start value is written by the pod backend once the values exist, and the command of the pod
  waits for it. The pod must set `command`, and its image needs a shell.
- A value that goes into a `regex: true` pattern is escaped.

References decide order. A participant is created after every participant whose `container_id` it reads in a place
fixed at create. It starts after every participant it connects to, once the endpoint shows its readiness evidence. A
cycle in either order is a plan error that names the chain.

## Addresses, wiring and routes

The harness owns the address of each endpoint: a port, a socket path or a pipe name. Which interface a listener binds
is the configuration of the software, and the harness never reads it.

A type wires its listeners through its default environment: an entry that carries an address is a reference to the
endpoint. An endpoint is usable when the final configuration of the participant references it, in `env`, `command`,
`args`, `programs` or a file that it places with `template: true`. An endpoint that a case declares is usable as
declared. Planning rejects a `connect`, a step or a readiness gate that uses an endpoint that nothing references, and the error gives
the line to write from `wiring`. References in `ready` and `commands` do not wire an endpoint.

The `agent` and `agent-adp` types withdraw a default environment entry in three cases, all by looking at which keys
the case sets and never at an address:

1. The `datadog.yaml` of the case sets the file key of the entry.
2. In a Linux container or pod, the `env` of the case sets `ADP_DD_<K>` for the default `DD_<K>`.
3. The `env` of the case sets another name that the software reads for the setting, or its `ADP_` form.

Two endpoints of one participant cannot share an address. The ports of `tcp`, `http` and `grpc` are one space, `udp`
ports are another, a socket path is one space for both Unix protocols, and pipe names are compared without letter
case. All fixed host-process ports are below 49152, so that they stay clear of the ports that macOS hands out.

`connect` checks, orders, gates and delivers. A name that the type declares in `consumes` checks the protocol and adds
the environment. Any other name delivers nothing: the participant reads `{{ self.connections.<name>.<part> }}` itself.
A connection that nothing uses is an error.

A route is chosen per protocol, consumer realization and server realization. Two participants of one realization
connect: by name in containers and pods, over loopback for host processes, and through the directory of the server
for Unix sockets. Two realizations, a Unix socket in a Windows container, and a named pipe are plan errors.

A step, a readiness gate or an operation reaches an endpoint from the runner, and the way depends on the realization
of the server.

| What is done | Linux container | Pod | Windows container | Host process group |
| --- | --- | --- | --- | --- |
| Probe `tcp`, `http`, `grpc` or a Unix socket | courier in the server | courier in the server | courier in the server | in the runner |
| Probe a `named_pipe` | plan error | plan error | courier in the server | plan error |
| Send bytes | courier in the server | plan error | courier in the server | in the runner |
| Make an `http` request | published port | forwarded port | courier in the server | in the runner |

The courier lists the IPv4 addresses of a name first. On a Windows `nat` network a name resolves to a link-local IPv6
address before its IPv4 address, and a datagram to the IPv6 address is lost without an error.

The runner of the Windows job is itself a container, and it reaches no participant address. Everything that touches
a participant there goes through the courier in the participant. Do not assume a published port. A probe from the
outside of a Linux container proves nothing, because the port proxy of Docker accepts a connection when nothing
listens inside.

## Realization backends

A backend takes a planned participant and realizes it. It has two seams: `Facts`, the static facts that planning
reads, and a run-time participant that creates, starts, observes and removes. A backend also rejects at plan time
what it cannot honor. There are three backends for four realizations. One container backend covers Linux and Windows
containers: their differences are the fields of `Facts`, which are the shared root and separator, the path of the
courier, whether the case has a volume, whether ports publish, whether host paths mount, whether a command sees the
end of its input, and the options that a case may set.

| | Linux container | Windows container | Pod | Host process group |
| --- | --- | --- | --- | --- |
| Per case | a bridge network, a volume | a `nat` network | a namespace, a directory on the kind node | `/tmp/panoramic` |
| `{{ self.dir }}` | `/airlock/<p>/shared` on the volume of the case | `C:\airlock\<p>\shared`, in the container only | `/airlock/<p>/shared` on the node directory | `/tmp/panoramic/<p>/shared` |
| Courier | copied in with the files | copied in with the files | mounted from the node | the library, in the harness |
| Stop | stop with a grace, force-remove | the same | delete the pod and the Service | SIGTERM to the group, SIGKILL after 2 s |

Points that the table leaves out:

- A Windows container needs a Windows runner host. A host process group on Windows is refused, because stopping a tree
  of processes there needs a Job object.
- The payload of a send is an argument in a Windows container, because the exec client cannot send the end of its
  input. `Facts::exec_input_ends` is false there. A payload over 15 000 bytes is a harness error.
- Linux containers follow a log file with the courier's `follow`, so no case image needs `sh` or `tail` for it.
- A pod is submitted in the start phase. A pod waits for late files only when it has a template file with a value
  known after start.
- The state directory of the host backend is one fixed path, short enough for a socket on macOS. Preparing refuses a
  directory that exists. A run killed with SIGKILL leaves it behind.
- A host program runs with a cleared environment, plus `PATH`, `HOME` and `TMPDIR` of the runner.
- Cases with host processes run one at a time: the scheduler holds one permit that such a case takes.

The lifecycle runs six phases: prepare, create, start, procedure, observe and reclaim. The ledger records each
resource before the backend makes it, and reclaim removes everything on every path: pass, failure, error, timeout and
cancellation. Only consumers wait for readiness. A handle operation says whether a problem is the software's or the
harness's. The first makes a step `failed`, and the second makes it `errored`.

## Observation and exit

Every process has one exit channel and one output channel.

| Process | Exit | Output |
| --- | --- | --- |
| A host process | The wait result of the child | Its stdout and stderr pipes |
| The only process of a container or pod | The exit of the container | The log of the container |
| A supervised process | A record file that the supervisor appends to | A log file that the backend follows |

- An exit is running, exited with a status, or undetermined. Undetermined is an observation error: a step that needs
  the status errors, and no code is invented.
- A supervisor appends one JSON object per exit to `$PANORAMIC_EXIT_DIR/<name>`, with `code`, `signal` and `time`.
  A death by signal has code 256 and the signal number. The first line is final, so a restart never returns a process
  to running. [Panoramic cases](./panoramic.md#observation) documents the format.
- The Windows entrypoint starts the Agent and then ADP, appends one record each, restarts nothing, and keeps the
  container up after both exit.
- A host process group reports the exit of each program (`Realized::reported_exit`), and `Handle::exit` asks it first.
- A participant whose processes are all supervised also gets its container log, as `_container.log`. It is observed
  as `_container`, so a `log_contains` with no `process` searches it with every process.
- `keep: always` and `keep: on_failure` are data on a collected file. The runner knows no file names.

## Steps and operations

A step is a registered name and a struct that is read from the fields of the case. `StepFactory` builds a `Step` from
`StepFields`, and does no I/O. `Step::needs` lists everything of the case that the step uses, as `Need` values, so
planning checks them before anything starts. `Step::run` gets a `StepContext`, which holds a `Handle` for each
started participant. A step never learns how a participant is realized.

An `Operation` is a capability that a participant offers: a name, the commands and endpoints of its own participant
that it uses (`uses`), and a function. Planning checks each use. An operation is written once, with no list per
realization, and it runs wherever the commands and endpoints that it uses exist.

The rule for choosing: a plain command line needs no Rust, so it is an entry of `commands` that `exec` runs. An
operation exists where the answer must be decoded, or the arguments are the own arguments of the software.

Knowledge about one piece of software stays with its type. The `commands` of a spec name its command lines for each
realization, and nothing else in the harness writes a path to a program of the Agent or ADP.

A check that only one case needs is a script that the case supplies, run with `exec`. It is not a step.

## Testing the harness

The cases are the main test. A test of the harness covers what a passing case cannot show.

| Suite | Shows | Lives |
| --- | --- | --- |
| Bad cases | A bad case file is rejected, for a given subject OS, with a given message. | `testdata/bad/`, with `expected.yaml` and one driver test |
| Examples | An example plans for every OS it declares, and two plans of one example are identical. | `testdata/examples/` |
| Real cases plan | Every case under `test/` plans for every OS it declares. | One test in `main.rs` |
| Failing checks | A step fails when its condition is false, and errors when the mechanism fails. | Beside each step, against one scripted handle |
| Cleanup and the scheduler | Deadline, cancellation and reclaim leave nothing behind. | With the runner, the ledger and the tasks |
| Cleanup in containers and pods | A failed start and a cancelled case leave nothing behind. | Ignored tests. `make test-container-daemon` and `make test-kind` run them. |
| The boundary | The rule under [Modules](#modules). | `test/architecture` |

- A test that needs a participant writes one in YAML with no participant type. There are no fake participant types.
- No test pins the order of errors, or the text of help, listings or printed plans.
- The driver and the step tests are unit tests of the binary crate, because `make test` runs `--lib --bins` and skips
  a `tests/` directory.
- A test that needs a Docker daemon or a kind cluster is ignored. No CI job runs it.

## Departures from the first design

These choices differ from what the design said first. Each is in the code now.

- A step's `os` overlay merges a field whose value is a map on both sides by key, one level, and replaces the rest.
  This is the rule for a participant, with "map" read from the value and not from a list of fields.
- A family of cases that ran once per OS is one case with an overlay. The merged cases unify the values that only
  label an OS: `DD_HOSTNAME`, `DD_API_KEY` and the metric names are those of the Linux case. Each OS keeps its own
  paths, commands and timeouts. The `adp-logging-default-path`, `-ignores-core-agent-log-file` and
  `-respects-data-plane-log-file` cases check the same things on each OS, and the last two end with a 1-second
  `process_stable_for` on every OS. The "ignores" case checks the Core Agent's file and then the ADP file on every OS.
- `dogstatsd-replay-unix` and `dogstatsd-replay-windows` are `dogstatsd-replay`.
- `datadog-intake` as host processes runs `target/release/datadog-intake`, which `build-datadog-intake-host` builds.
  In a Windows container it runs from `saluki-images/datadog-intake:testing-windows`, which the Windows script builds
  from the base of the ADP image.
- `agent-adp` on macOS and Windows consumes `intake` with `DD_DD_URL` and `DD_APM_DD_URL` only.
- The subject of `case-supplied-scripts` is a participant with no participant type. The case runs no ADP.

## Left for later

Each of these waits for the first case that needs it.

- Assigned addresses for cases with host processes that run in parallel. Addresses are fixed today.
- A subject that runs as host processes with helpers in containers. Two realizations in one case have no route.
- Host process groups on Windows.
- A way to match a signal in the same way on every realization. `exits_with` matches a code.
- `diagnostics`, which no case needs yet.
- The exited-sender variant of the comparison cases.
- `exec` with `env` in a pod.
- Fixes to the pod backend that the kind tests show.
- One wait for the load that the comparison cases share.
- A host descendant that leaves its process group, for example with `setsid`, gets no signal and may outlive the case.
- A test of the step driver for a step that overruns its budget.

:::
