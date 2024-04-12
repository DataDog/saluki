# dogstatsd-client

This client is used to generate valid DogStatsD payloads using the [official Datadog Go library][dsdgo] which can be
used for testing both the correctness of the DogStatsD codec as well as ensuring full support for all valid
transports, origin detection, and so on.

## Usage

### Build the client

First, build the client:

```shell
make build-dsd-client
```

### Basic testing scenarios

There are three existing Make targets for running a "basic" workload (one of each metric type: count, gauge, histogram,
distribution, and set) against each valid transport: UDP, UDS in datagram mode, and UDS in stream mode. You can read the
`Makefile` to see the exact addresses used when running these basic scenarios:

```shell
make run-dsd-basic-udp
make run-dsd-basic-uds
make run-dsd-basic-udp-stream
```

Here's an example of running the basic UDP scenario, with a view of the metrics generated and sent:

```shell
# In one terminal:
$ make run-dsd-basic-udp
[*] Building Dogstatsd client...
[*] Sending basic metrics via Dogstatsd (UDP, 127.0.0.1:9191)...


# In another terminal:
$ nc -u -l localhost 9191
dsd_client.test.histogram:3|h|#source:dogstatsd-client
dsd_client.test.gauge:2|g|#source:dogstatsd-client
dsd_client.test.set:five|s|#source:dogstatsd-client
dsd_client.test.distribution:4|d|#source:dogstatsd-client
dsd_client.test.counter:1|c|#source:dogstatsd-client
```

### Running by hand

The client can also be used by hand as seen below:

```shell
$ tooling/bin/dogstatsd_client <address> <operations>
```

For the `address`, this can be one of the following:

- `<IP address>:<port>`, which implies UDP mode
- `unixgram:///path/to/socket`, which uses UDS in `SOCK_DGRAM` mode
- `unix:///path/to/socket`, which uses UDS in `SOCK_STREAM` mode

For `operations`, this is a comma-separated string of "operations" to execute, where each operation corresponds to a
specific metric that will be emitted. Below is an example:

```shell
$ tooling/bin/dogstatsd_client 127.0.0.1:8125 count:1
$ tooling/bin/dogstatsd_client 127.0.0.1:8125 count:1,gauge:-3
$ tooling/bin/dogstatsd_client 127.0.0.1:8125 set:five
```

Operations are in the form of `<metric type>:<metric value>`, and each metric type always ends up creating the same
exact metric name, following the pattern of `dsd_client.test.<metric type>`. The same operation can be specific multiple
times, which can be used to trigger multi-value payloads, as extended client-side aggregation is enabled on the client.

The following metric types can be used:

- `count`
- `gauge`
- `histogram`
- `distribution`
- `set`

[dsdgo]: https://pkg.go.dev/github.com/DataDog/datadog-go/v5/statsd