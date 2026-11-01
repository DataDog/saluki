# Panoramic s6 overlay experiment

## Question

Can files placed before container start replace a service's `finish` and add `log/run`, without changing the shipped
restart behavior? Can Panoramic read the resulting records and process logs both while the container runs and after
it exits? Which streams reach the logger, and how much time remains before s6 kills a slow `finish`?

**Yes, with stdout-only service logging.** On the tested image, recording an exit before the shipped body preserves
ADP's restart and clean-exit behavior. Records survive container shutdown. The shipped failure path takes about
2.005 s against a measured finish timeout of 5.014 s, leaving about 3.009 s. Capturing stderr in the process log
requires an explicit redirection in the service's `run` script.

## Reproducing

Run from the checkout root on macOS with bash, curl, Python 3 (standard library only), and a Linux Docker daemon.
The script sends every Docker operation through curl's Unix-socket transport.
Do not run copies concurrently: the cleanup label is shared by every reproduction.

The daemon must already contain `saluki-images/datadog-agent:testing-devel` and `alpine:latest`. `run` never pulls;
it reports the missing reference if either image is absent. Supply an existing converged image and fix-up image
with `IMAGE` and `FIXUP_IMAGE` if their tags differ. To load image archives supplied by a build machine:

```bash
export DOCKER_HOST="unix://$HOME/.lima/docker/sock/docker.sock"
socket=${DOCKER_HOST#unix://}
curl --fail --unix-socket "$socket" -X POST \
  -H 'Content-Type: application/x-tar' --data-binary @target/datadog-agent-image.tar \
  'http://d/images/load?quiet=1'
curl --fail --unix-socket "$socket" -X POST \
  -H 'Content-Type: application/x-tar' --data-binary @target/alpine-image.tar \
  'http://d/images/load?quiet=1'
```

The recorded run used existing images. `provenance` alone may pull: it reads the default tag from
`docker/Dockerfile.datadog-agent`, downloads the pinned upstream image if absent, and removes that image afterward.
An already-present image is reused and retained. `TAG` overrides the provenance tag.

These are the reproduction commands used:

```bash
cd docs/development/plans/panoramic-architecture-s6-experiment
DOCKER_HOST="unix://$HOME/.lima/docker/sock/docker.sock" ./s6-experiment.sh provenance
DOCKER_HOST="unix://$HOME/.lima/docker/sock/docker.sock" ./s6-experiment.sh run
DOCKER_HOST="unix://$HOME/.lima/docker/sock/docker.sock" ./s6-experiment.sh run
```

Each scenario gets a fresh named volume and container. An Alpine fix-up container creates
`/airlock/adp/{exit,log,shared}` and runs `chmod -R 777 /airlock`, then exits and is removed. Subjects have
`Hostname: adp`, `PidMode: host`, the volume mounted at `/airlock`, and the requested participant, API-key, hostname,
and data-plane environment variables. `TZ=UTC` makes `init.log` timestamps unambiguous. The overlay contains service
files; directory preparation belongs to the fix-up container.

The script refuses existing resources labelled `panoramic.experiment=s6`, printing removal commands without deleting
them. After preflight, an EXIT trap force-removes labelled containers, removes labelled volumes, and queries both
lists again. Any failed check or cleanup returns nonzero. Both recorded runs finish in under four minutes.

## Environment

Recorded on **2026-11-01**, macOS client, lima Linux daemon:

| Property | Value |
| --- | --- |
| Docker Engine / API | `29.4.2` / `1.54` |
| Daemon OS / architecture | `linux` / `arm64` |
| Kernel | `6.8.0-147-generic` |
| Subject reference | `saluki-images/datadog-agent:testing-devel` |
| Subject image ID | `sha256:d414eb1d21a8a55c934f783a22a88ea45359cde555b084a28821e22adf1b0f3f` |
| Subject Agent version / revision | `7.83.3` / `8c639c92581e6f5da73f90f1886b21b9a2441bca` |
| Subject ADP version / revision | `1.7.0` / `596b2d9302` (startup log) |
| Fix-up reference | `alpine:latest` |
| Fix-up image ID | `sha256:be2656061b536c1883c4034790e21d0a09c5aaaf51968b89368a3c0371ea2c9a` |
| Dockerfile pin | `registry.datadoghq.com/agent:7.85.0-rc.5-full` |
| Pinned image ID | `sha256:8d1549cd677fb1709d2f2068941b209b581760265d8e48d42ccf0ee2dee20bfa` |
| Pinned repo digest | `registry.datadoghq.com/agent@sha256:8d1549cd677fb1709d2f2068941b209b581760265d8e48d42ccf0ee2dee20bfa` |
| Pinned Agent version / revision | `7.85.0-rc.5` / `91d6d45b852f3742eec574bbc722b1606dd96223` |
| Image s6 environment | `S6_KEEP_ENV=1`, `S6_LOGGING=0`, `S6_READ_ONLY_ROOT=1`, `S6_BEHAVIOUR_IF_STAGE2_FAILS=2` |

The subject uses an older Agent base than the Dockerfile pin. Provenance compares the following files from
unstarted containers, using the archive API. Each SHA-256 is **identical in both images**:

| Image path | SHA-256, both subject and pin | Result |
| --- | --- | --- |
| `/etc/services.d/agent/finish` | `821d7b4cde8cc4b671c64e5755e1d464579fc973eddcda76f7a6528a22d50968` | PASS |
| `/etc/s6/init/init-stage2` | `272e091c9edbe4adea224eae4dd6a939ba7c078a0f0dc7df224fe17ea3c46316` | PASS |
| `/etc/s6/init/init-stage3` | `ea2d995df5a28709b2a040d2a212ebaa1e351c8233cc26cd4803fdc6df52d2b3` | PASS |

The subject data-plane finish and `docker/s6-services/data-plane/finish` also match, with SHA-256
`31fcf575e37410af632678e695c670ea686f2c78691182b58d9e93467db82967`. Every scenario checks this equality before overlaying.

These comparisons support transferring overlay placement and shutdown behavior to the pin. Product-specific
startup and shutdown behavior still needs testing with a converged image built from that pin.

## Results

The [recorded transcript](./panoramic-architecture-s6-experiment/results/2026-11-01-arm64.txt) contains provenance and
two consecutive successful runs. The table and measurements below use the second run. S1's stderr comparison is
skipped because the stream is empty; the synthetic S4 checks exercise both streams. All exercised checks pass.

| Check | Result | Excerpt or observation |
| --- | --- | --- |
| S1: an ADP exit with code 1 appends a record before the shipped finish body runs | PASS | `1 0 1793575631.274329308`; shipped log timestamp `1793575631.275404`, about 1.07 ms later |
| S1: ADP is restarted about two seconds after each nonzero exit | PASS | Record-to-next-start samples `2.016255`, `2.003505`, `2.004855` s |
| S1: adp.log grows while the container log still shows the same ADP lines | PASS | `2851 -> 11916` bytes; both contain `Unsupported configuration key with non-default value` for `heroku_dyno` |
| S1: agent.log grows while the container log still shows the same Agent lines | PASS | `21841 -> 29281` bytes; both contain `Using '/opt/datadog-agent/embedded' as Python home` |
| S1: adp.log contains ADP's stdout and not container stderr | SKIP | Not exercised: container stderr has zero lines. S4 answers the stream question |
| S1: adp.log also contains the run and finish scripts' own log lines | PASS | `starting agent-data-plane`; `agent-data-plane exited with code 1, signal 0, restarting in 2 seconds` |
| S1: stopping the container while ADP is running appends a signal record | PASS | ADP held with SIGSTOP; `256 15` at `1793575654.707500`; stop `2.559292` s with `t=10`; container exit `0` |
| S1: exit records and process logs are readable from the stopped container | PASS | Archive reads succeed for both exit files and both process logs |
| S2: SIGKILL to ADP records 256 9 and s6 restarts it with a new pid | PASS | `256 9 1793575686.266714567`; pid `149889 -> 151090` |
| S2: SIGTERM to a started ADP records its exit code and the shipped finish disables the service when the code is 0 | PASS | `0 0 1793575712.851001104`; `down (exitcode 0)`; `up,wantedup=false false`; no new record for 10 s |
| S2: SIGKILL to the Agent records 256 9 for agent and the container stops itself | PASS | Agent: `256 9 1793575733.154521359`; container exit `0`; ADP during shutdown: `0 0 1793575733.995839864` |
| S2: records are readable after the container stopped itself | PASS | Archive reads return all three ADP records and the Agent record |
| S3: a finish script that overruns is killed and ADP is restarted, and its record is kept | PASS | First record remains; next start after `5.013507` s despite `s6-sleep 30`; `timeout-finish` absent |
| S4: s6 pipes only stdout to a service logger | PASS | `probe.log` contains only `panoramic-probe-stdout`; `panoramic-probe-stderr` appears only in container stderr |
| S4: redirecting stderr in the run script sends both streams to the logger | PASS | `probe-merged.log` and container stdout contain `panoramic-probe-merged-stdout` and `panoramic-probe-merged-stderr`; neither appears in container stderr |
| S5: a failed record write does not change restart behaviour | PASS | `redirfd: fatal: unable to open /airlock/adp/exit/data-plane: No such file or directory`; pid `154929 -> 155539`; restart after `2.007652` s |
| S5: record-write errors reach container stderr only | PASS | The `redirfd: fatal` line appears in container stderr and is absent from adp.log and container stdout |
| S6: stopping a healthy container records clean exits for ADP and Agent | PASS | ADP: `0 0 1793575808.932571363`; Agent: `0 0 1793575808.232153144`; stop `2.617291` s; container exit `0` |
| Every scenario: image data-plane finish matches the repository body | PASS | Six comparisons before overlay placement |
| Active service finish files have the record prefix and shipped body byte for byte | PASS | Eleven reads from `/var/run/s6/services/<svc>/finish` after start, checked against the prefix and independently captured image body |
| S3: data-plane finish replaced by timeout variant | INFO | Record prefix followed by `s6-sleep 30`; excluded from shipped-body equality checks |
| Provenance: all three upstream hashes match the subject | PASS | Hashes above; saved Agent finish copy has the same hash |
| Provenance: remove the image pulled for inspection | PASS | Image lookup by ID returns absent after deletion |
| Cleanup: no labelled containers or volumes remain | PASS | Both runs end with `containers=[] volumes=[] errors=[]`; independent API queries confirm this |

### Measurements

| Measurement | First run | Second run |
| --- | --- | --- |
| Crash-record interval, median | 2.246960 s | 2.238703 s |
| SIGKILL timestamp to exit record | 0.004039 s | 0.003883 s |
| Shipped failure finish duration, median, record to next start | 2.004805 s | 2.004855 s |
| Finish timeout, record to next start | 5.016939 s | 5.013507 s |
| Timeout minus shipped duration | 3.012134 s | 3.008652 s |
| Docker stop, crash loop, ADP held with SIGSTOP | 2.621496 s | 2.559292 s |
| Docker stop, healthy ADP and Agent | 2.594804 s | 2.617291 s |

Record and signal timestamps come from `date +%s.%N` inside the container. Next-start and shipped-log timestamps
come from `/initlog.sh`'s nanosecond `init.log` timestamps, interpreted in UTC. Numeric differences are rounded to
microseconds; the exit files themselves retain nine fractional digits. Host monotonic time is used only for poll
deadlines and sleeps, not these in-container measurements.

The stop interval starts at an exec timestamp immediately before the stop request and ends at Docker's
`State.FinishedAt`, on the same Linux VM clock. It includes exec-inspection and request overhead and is therefore
conservative. The transcript prints that pre-request timestamp as `before_stop`. Kill-to-record is an upper bound
on exit-to-record latency: **at most 3.883 ms** in the second run. The shipped finish duration includes record
completion and supervisor scheduling before the next run script logs its start.

### Experimental controls

- S1 obtains the supervised pid and sends SIGSTOP in one exec, holding the short-lived crash-loop process alive
  until Docker stop. This tests signal handling when the service process is running. S6 separately tests a healthy
  stop; its `0 0` records are indistinguishable from spontaneous clean exits except by their relation to the stop time.
- `Agent Data Plane running.` marks healthy startup. The script counts these lines across restarts and waits for
  s6-reported uptime of at least 20 s before the direct signal tests and healthy-stop test.
- `s6-svstat -o up,wantedup` reports `false false` when the clean-exit finish has disabled the service. This checks
  current and desired state directly.
- After ADP stays down for 10 s following SIGTERM, `s6-svc -u` re-enables it. A fresh running marker ensures the
  subsequent Agent kill has a running ADP to shut down.
- Removing the exit directory forces the record write to fail even for root, which can bypass mode `000`.

## Provenance of upstream/agent-finish

The [saved file](./panoramic-architecture-s6-experiment/upstream/agent-finish) is a byte-for-byte archive-API copy,
made on **2026-11-01**, of `/etc/services.d/agent/finish` in:

- Reference: `registry.datadoghq.com/agent:7.85.0-rc.5-full`
- Repo digest: `registry.datadoghq.com/agent@sha256:8d1549cd677fb1709d2f2068941b209b581760265d8e48d42ccf0ee2dee20bfa`
- Revision label: `91d6d45b852f3742eec574bbc722b1606dd96223`
- SHA-256: `821d7b4cde8cc4b671c64e5755e1d464579fc973eddcda76f7a6528a22d50968`

The data-plane finish body comes from `docker/s6-services/data-plane/finish`, verified against the subject image.
The Agent body comes from the subject container. Each composition preserves the shebang, inserts the record block,
and appends the shipped bytes from line two onward. After start, the script checks the active s6 copies against
those independently captured bodies. The saved upstream copy records the source for the `agent-adp` definition's
s6 overlay assets.

## Findings for the implementation

- **One appended line per exit:** `<code> <signal> <seconds>.<nanoseconds>`, with nine fractional digits. Signal death
  is `256 <n>`; a graceful SIGTERM can produce `0 0`. The Agent SIGKILL case records `256 9` while the container exits
  `0`, so process status must come from the service record.
- **Contain the entire record operation in one `foreground` block, including `importas` and `date`.** This keeps
  import, timestamp, and redirection failures inside the child execution so the shipped body can run. Removing the
  destination directory demonstrates containment for a failed write.
- **Shipped s6 service logs are stdout-only. Tagging them `merged` is incorrect.** `tee -a` appends stdout to the
  process log while preserving container stdout. A merged `adp.log` requires `fdmove -c 2 1` in an overlaid `run`
  script. Errors from `finish` and `record-exit` reach only container stderr, as S5's `redirfd: fatal` line shows.
- **adp.log includes service-script messages.** `starting agent-data-plane` and the shipped finish messages share
  the logger with ADP stdout. Process-log consumers must account for these lines.
- **A harness stop may produce `256 15`, `0 0`, or no additional record.** It appends an exit record only for a
  service whose process is running when stop arrives. Healthy ADP and Agent processes record `0 0`, just like a
  spontaneous clean exit. A service already in `finish` or down records nothing more. Readers must not wait for a
  stop record: use the stop-request time to distinguish stop-induced exits from earlier failures. Both harness
  stops and Agent-triggered shutdown allow archive reads of existing records from the stopped container's mounted
  volume.
- **Use s6 to locate service pids in host-PID containers.** `pgrep` sees unrelated host processes. The service path
  `/var/run/s6/services/<service>` identifies the process through `s6-svstat`.
- **Backend-prepared directories suffice.** The fix-up container creates writable record and log directories before
  service startup; the overlay contains only service files.
- **The timing budget is finite.** A 30-second finish is killed after about five seconds and retains its early
  record. The shipped failure body leaves about three seconds of margin. A blocking record operation can consume
  that margin before the shipped body runs.
- **Product-version coverage is bounded by the tested image.** The pinned Agent finish and s6 init scripts match
  exactly, while the running converged image uses the older Agent and ADP builds listed above. Repeat the experiment
  with a rebuilt image before relying on product-specific exit behavior.
