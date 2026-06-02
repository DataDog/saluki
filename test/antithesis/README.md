# Antithesis Tests

This directory contains a sub-project to run Antithesis tests for the saluki
project. Primary focus is on establishing that ADP and DogStatsD behave
'equivalently', which is to say if ADP is dropped in for Datadog Agent's
DogStatsD users will not notice shifts in their telemetry. Better operational
behavior is acceptable deviation.

## Prerequisites

* snouty -- https://github.com/antithesishq/snouty
* antithesis-skills + claude -- https://github.com/antithesishq/antithesis-skills

## Running Scenarios

This effort is extremely early. Today we assume claude drives scenarios runs,
command it to do so with `/antithesis-launch`. In order for this to work you
must already have credentials available. Eventually we will have CI rigged up to
do nightly shots.
