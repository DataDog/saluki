# Agent Data Plane

Agent Data Plane is the reference data plane built on top of Saluki. It's designed to replace specific functionality of
the [Datadog Agent](https://github.com/DataDog/datadog-agent) related to handling telemetry data, such as metrics, logs,
and traces, while providing a number of benefits over the existing Datadog Agent implementation, such as improved
performance and deterministic resource usage.

This section of the documentation covers specific areas related to the development process of Agent Data Plane, as the
code lives within the Saluki repository. It won't be relevant to those simply looking to contribute to or utilize
Saluki for their own purposes, but is meant to live alongside the ADP codebase to ensure that all relevant information
is quickly and easily accessible.

Use [Inspect retained DogStatsD contexts](dogstatsd-top.md) to snapshot retained aggregation contexts, investigate
cardinality, and analyze sensitive dump artifacts offline.
