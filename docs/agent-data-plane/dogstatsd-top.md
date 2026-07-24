# Inspect retained DogStatsD contexts

Use `agent-data-plane dogstatsd top` to inspect which DogStatsD metric contexts Agent Data Plane (ADP) currently retains and to identify metric names with high cardinality.

## Understand what `top` reports

`top` snapshots the contexts currently retained by the `dsd_agg` aggregate transform. It does not measure packet or sample frequency, and it does not collect data over a time window. Use the separate `agent-data-plane dogstatsd stats` command when you need time-windowed sample counts and last-seen times.

A context's full identity consists of its metric name, host, client tags, and origin tags. The report groups these contexts by metric name, so contexts that differ only by host or origin tags still increase the context count for that name. The displayed tag cardinalities include only client tags; they do not include hosts or origin tags.

The report reflects Saluki's normal aggregate lifecycle. Contexts disappear after their aggregation state is flushed and no longer retained. Sparse counters can remain as idle counters while the aggregate transform emits zero values, then disappear according to the configured Saluki counter expiry. Do not interpret this behavior as an Agent-identical context time to live.

## Request and analyze a dump

Run `top` without `--path` to request a new dump and analyze it:

```console
$ agent-data-plane dogstatsd top
Wrote /var/run/datadog/dogstatsd_contexts.json.zstd
   Contexts	Metric name	(number of unique values for each tag)
          3	request.count	(2 env, 1 service)
```

Online requests read the configured Agent IPC authentication token and pin the API server certificate to the configured IPC certificate. The API creates and returns only the server-local path `<run_path>/dogstatsd_contexts.json.zstd`.

The API does not return the artifact contents. The CLI reads the returned path directly, so the CLI process and ADP must see the same filesystem and path. If ADP runs in a container, run the CLI with the same mount or use the copy workflow in the next section. The API does not provide a remote download endpoint.

Use `--num-metrics` (`-m`) and `--num-tags` (`-t`) to change the default limits of 10 metric names and 5 tag keys:

```console
$ agent-data-plane dogstatsd top --num-metrics 20 --num-tags 8
$ agent-data-plane dogstatsd top -m 20 -t 8
```

The legacy `--mum-tags` spelling remains an alias for `--num-tags`. Do not pass both spellings in one command. Limits must be non-negative integers, and `top` rejects extra positional arguments.

The report orders metric names by descending context count and tag keys by descending unique-value count. It resolves ties lexically. When only one item exceeds a limit, the report displays that item; when two or more exceed it, the report replaces them with one remainder row or tag summary.

To create the artifact without rendering a report, run:

```console
$ agent-data-plane dogstatsd dump-contexts
Wrote /var/run/datadog/dogstatsd_contexts.json.zstd
```

`dump-contexts` takes no additional arguments. A successful request atomically replaces any prior file at the fixed path. The completed artifact remains there until an operator removes it.

## Analyze a copied dump offline

Wait for `top` or `dump-contexts` to report success, then securely copy the completed artifact to the analysis host. Analyze the copy by passing its path explicitly:

```console
$ agent-data-plane dogstatsd top --path /secure/cases/incident-123/dogstatsd_contexts.json.zstd
```

You can combine `--path` (`-p`) with the metric and tag limits:

```console
$ agent-data-plane dogstatsd top -p /secure/cases/incident-123/dogstatsd_contexts.json.zstd -m 25 -t 10
```

Offline mode reads only the supplied file. It does not contact ADP or read the Agent IPC authentication token or certificate. `top` requires the `--path` value and does not fall back to a file in the current working directory.

## Interpret context and tag counts

Each artifact record represents one full retained context. The **Contexts** column counts records with the same metric name, including contexts distinguished only by host or origin tags.

For the parenthesized tag summary, `top` deduplicates client tags across all records for the metric name. It treats the text before the first colon as the tag key and counts unique complete tag values. It treats a tag without a colon as its own key. For example, `(3 pod_name, 2 env)` means the retained contexts contain three distinct client tags whose key is `pod_name` and two whose key is `env`. It does not mean that ADP received three or two samples.

Use the remainder row to account for hidden contexts. For example, `17 (other 4 metrics)` means the four hidden metric names contain 17 retained contexts in total. A tag remainder such as `12 values in 3 other tags` reports the summed unique-value counts for the hidden tag keys.

## Protect and remove dump artifacts

> [!WARNING]
> DogStatsD context dumps can contain metric names, hosts, client tags, and origin tags that reveal tenant or workload details. Treat every dump and copy as sensitive diagnostic data.

On Unix, ADP creates the artifact with mode `0600`, owned by the ADP process owner. On Windows, the artifact inherits the configured `run_path` directory ACL, matching the Datadog Agent; secure a custom `run_path` for the ADP service identity and intended administrators before requesting a dump. Keep the resulting access restriction when you copy or store the file, use an encrypted and access-controlled transfer method, and avoid placing it in shared temporary directories. Remove the server-side artifact and every copied artifact manually when you finish the investigation.

A later successful dump replaces the fixed server-side file but does not remove copies elsewhere. ADP does not apply a retention or cleanup policy to the completed file.

## Understand snapshot consistency

ADP currently has one `dsd_agg` owner. That owner constructs an internally consistent snapshot of its retained context map and then resumes processing. ADP serializes and compresses the snapshot after the owner resumes, so file creation does not hold the owner for the serialization work.

Snapshot construction uses memory proportional to the number of retained contexts, or O(contexts), in addition to the aggregate state itself. Serialization also retains that snapshot until publication finishes.

If ADP uses multiple aggregate owners in the future, each owner's snapshot remains internally consistent. The combined artifact would be a rolling snapshot because owners can respond at different moments; it would not represent one atomic instant across all owners.

## Troubleshoot errors

Use the status or file error in the CLI output to choose a response:

- **401 Unauthorized**: The configured Agent authentication token does not match the server token. Verify that the CLI and ADP use the same current token file. Online commands also fail before the request if they cannot read the token or certificate, or if certificate pinning rejects the server certificate.
- **404 Not Found**: The context-dump route is absent when DogStatsD is disabled. Also verify that the CLI targets the intended ADP privileged API endpoint.
- **503 Service Unavailable**: The aggregate snapshot owner is not running or stopped before it responded. Wait for the topology and `dsd_agg` to become healthy, then retry.
- **504 Gateway Timeout**: The aggregate owner did not return a snapshot within the 30-second API snapshot deadline. Check ADP health and load before retrying.
- **500 Internal Server Error**: ADP could not publish the artifact. Verify that `run_path` is configured, exists as a directory, and is writable by the ADP process. An empty `run_path` fails; ADP does not fall back to the current working directory.
- **Missing or unreadable path**: An online request returns a path on ADP's filesystem. If the request succeeds but `top` cannot open that path, give the CLI access to the same filesystem or use `dump-contexts`, copy the completed file, and run offline with `--path`.
- **Malformed or truncated artifact**: A zstd initialization error or a JSON record error usually indicates an incomplete, corrupt, or unsupported file. Copy the artifact again after dump publication completes and verify the transfer before retrying. Pass the exact file path because offline mode has no current-working-directory fallback.

## Artifact interoperability

ADP writes a zstd-compressed stream of newline-delimited JSON (NDJSON) without a schema version. Every record uses the Agent diagnostic schema with these exact PascalCase fields:

```json
{"Name":"request.count","Host":"node-a","Type":"Counter","TaggerTags":["pod_name:web-a"],"MetricTags":["env:prod","service:web"],"NoIndex":false,"Source":1}
```

`TaggerTags` contains origin tags and `MetricTags` contains client tags. The Datadog Agent and ADP `dogstatsd top --path` command-line clients can each read artifacts produced by either implementation. Saluki detects zstd compression from the file's magic bytes rather than its filename, and it also accepts plain NDJSON with the same records.

This schema has no version field and serves as a diagnostic interoperability contract. It is not a supported metric ingestion API; do not send these records to a DogStatsD listener or depend on them as a general-purpose storage format.
