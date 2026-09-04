# Inspect the supervision tree

Use `agent-data-plane debug runtime show-processes` to see which processes Agent Data Plane (ADP) is running, how they
are nested, which have restarted, and how much memory and CPU each accounts for.

## Understand what the tree reports

ADP's runtime is a supervision tree: every unit of work is a *process* supervised by a *supervisor*, and supervisors
nest to form the tree. That structure is the most complete description of what ADP is doing, so reporting it answers
questions that logs answer only indirectly: whether a subsystem is running, whether it has been restarting in a loop,
and which part of the tree is accumulating memory.

Each node reports:

| Field | Meaning |
| --- | --- |
| `name` | The process's name, as registered with its supervisor. |
| `kind` | Whether the node is a supervisor or a leaf worker. |
| `state` | `running`, `exited` (ran and was not restarted), or `registered` (declared but not currently running). |
| `process_id` | The identifier of the node's current process. A restart produces a new one. |
| `process_name` | The fully qualified, dot-scoped process name. |
| `restart` | The node's restart policy: `permanent`, `transient`, or `temporary`. |
| `created_at` | When the node first became part of the tree. Constant across restarts. |
| `started_at`, `uptime_ms` | When the node's current process started, and how long it has been running. |
| `restart_count` | How many times the node has been restarted since it was created. |
| `resources` | Allocation and CPU accounting, for supervisors. |
| `supervision` | Restart strategy, shutdown budget, and runtime placement, for supervisors. |

Two details are worth knowing before reading the numbers.

> [!NOTE]
> Resource figures are reported per supervisor, not per worker. A worker inherits its supervisor's resource group
> rather than owning one, so its allocations are already counted against the supervisor named in its `resource_group`
> field. Worker nodes therefore name a group but carry no figures of their own.

> [!NOTE]
> Allocation figures require the tracking allocator, and CPU figures require per-thread CPU accounting. When either is
> unavailable, the counts read zero. Check the `resource_tracking_enabled` field, or the `resource tracking off` line
> in the tree output, to tell nothing allocated from nothing measuring.

The difference between `created_at` and `started_at` is the time a node has spent *not* running since it was created,
which is the figure worth looking at when something is restarting repeatedly.

A statically declared process that exits without being restarted stays in the tree as `exited`, because it remains
part of ADP's declared shape. Dynamically spawned processes, such as one per connection, are removed once they finish,
so the tree does not grow without bound.

## Show the tree

```console
$ agent-data-plane debug runtime show-processes
Supervision tree for 'adp-root', captured 2026-09-03T12:00:00Z
  52 processes (12 supervisors, 40 workers): 51 running, 1 exited, 0 registered
  3 restarts across the tree, max depth 5
  resource tracking on: 24.1 MiB live, 12.3s CPU

adp-root  [sup] running  pid=1  up=2h15m  one_for_one(0/5s)  live=1.2 MiB
|-- app-bootstrap  [sup] running  pid=3  up=2h15m  one_for_one(1/5s)  live=104.0 KiB
|   |-- logging-override  [worker] running  pid=4  up=2h15m
|   `-- runtime-metrics  [worker] running  pid=6  up=2h15m
|-- internal-sup  [sup] running  pid=8  up=2h15m  one_for_one(1/5s)  live=890.0 KiB
|   `-- ctrl-pln  [sup] running  pid=9  up=2h15m  restarts=1  one_for_one(1/5s)  rt=1thr  live=612.0 KiB
`-- primary  [worker] running  pid=20  up=2h15m
    `-- topology.primary  [sup] running  pid=21  up=2h15m  one_for_one(0/5s)  live=18.4 MiB
```

Indentation and branch glyphs show the nesting. A supervisor's line also carries its restart strategy as
`mode(intensity/period)`, and `rt=Nthr` when it runs on its own runtime rather than its parent's.

Nodes that need attention stand out without needing color: `restarts=` appears only when a node has actually
restarted, and a stopped process is marked `EXITED` in capitals.

## Change the output format

Use `--format` (`-f`) to select the output format. The default is `tree`.

Pass `--json` (`-j`), or `--format json`, to get the endpoint's payload unchanged. The payload is passed through
verbatim rather than re-serialized, so fields the CLI does not yet know about still reach whatever consumes it:

```console
$ agent-data-plane debug runtime show-processes --json | jq '.totals'
$ agent-data-plane debug runtime show-processes --json | jq '.. | objects | select(.restart_count > 0) | .process_name'
```

Pass `--format dot` to emit a [Graphviz](https://graphviz.org) graph. Supervisors are drawn with a bold border,
stopped processes are filled, and processes that are declared but not running are dashed:

```console
$ agent-data-plane debug runtime show-processes --format dot | dot -Tsvg > tree.svg
```

## Endpoint

| Route | Method | Description |
| --- | --- | --- |
| `/runtime/processes` | `GET` | A snapshot of the supervision tree, as JSON. |

The route is served on the privileged API endpoint (`data_plane.secure_api_listen_address`, `tcp://0.0.0.0:5101` by
default), which requires the IPC client certificate. A snapshot names every process and reports its resource
accounting, so it is authenticated the same way as the other state dumps such as `/config` and `/config/runtime`.

The same snapshot is collected into support flares as `supervision_tree.json`.

> [!TIP]
> Process names are shared across ADP's registries, so a snapshot joins against `/memory/status`, `/ready`, and
> `resources.json` on the `process_name` field with no translation.

> [!NOTE]
> A supervisor running on its own runtime, such as `ctrl-pln`, reports an unscoped process name (`ctrl_pln` rather
> than `adp_root.internal_sup.ctrl_pln`), because it re-roots its name when it starts. `/memory/status` additionally
> lists a scoped entry for such a supervisor that always reads zero. Both are known defects; the tree reports the
> name that the supervisor's allocations are actually attributed to.
