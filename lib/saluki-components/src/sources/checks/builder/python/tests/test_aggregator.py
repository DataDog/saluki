#!/usr/bin/env python3
"""Simple test script for aggregator module - to be executed from Rust tests."""

import aggregator

# Test basic metric submission
print("Testing aggregator.submit_metric...")
aggregator.submit_metric(
    None,  # class placeholder
    "test-check-id",
    aggregator.GAUGE,
    "test.gauge.metric",
    42.5,
    ["env:test", "source:python"],
    "test-hostname",
    False,
)

# Test different metric types
aggregator.submit_metric(
    None,
    "test-check-id",
    aggregator.COUNTER,
    "test.counter.metric",
    10.0,
    ["type:counter"],
    "test-hostname",
    False,
)

# Test service check
print("Testing aggregator.submit_service_check...")
aggregator.submit_service_check(
    None,
    "test-check-id",
    "test.service_check",
    0,  # OK status
    ["service:test"],
    "test-hostname",
    "All systems operational",
)

print("Python aggregator tests completed successfully!")
