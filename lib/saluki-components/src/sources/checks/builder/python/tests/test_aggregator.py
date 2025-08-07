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

# Test event submission with all fields
print("Testing aggregator.submit_event with all fields...")
test_event_full = {
    "msg_title": "Test Event Title",
    "msg_text": "This is a test event message from Python",
    "event_type": "info",
    "timestamp": 1234567890,
    "api_key": "test-api-key",
    "aggregation_key": "test-aggregation-key",
    "alert_type": "warning",
    "source_type_name": "python_test",
    "host": "test-event-hostname",
    "tags": ["event:test", "source:python", "priority:high"],
    "priority": "low",
}

aggregator.submit_event(None, "test-check-id", test_event_full)

# Test event submission with minimal fields
print("Testing aggregator.submit_event with minimal fields...")
test_event_minimal = {
    "msg_title": "Minimal Event",
    "msg_text": "This event has only required fields"
}

aggregator.submit_event(None, "test-check-id", test_event_minimal)

print("Python aggregator tests completed successfully!")
