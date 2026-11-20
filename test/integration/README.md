# ADP integration tests

Each directory under `cases/` is one integration case. An integration case checks the behavior of ADP on its own:
it starts, stays up, logs what it should, listens on its ports, exits as expected, and forwards what it receives to a
mock intake. A case runs on each OS that its `subject_os` lists: Linux, macOS, Windows, or more than one.

The correctness cases under `test/correctness/cases/` have another purpose. Each one runs ADP and the Datadog Agent on
the same load and compares their output.

## Running the cases

```bash
make test-integration          # build the images, run every case on Linux
make test-integration-quick    # run every case, with the images that exist
make list-integration-tests    # list the cases
make test-integration-macos-ci # on macOS: run the cases that declare macOS
```

On Windows, `ci/tooling/windows-integration-tests.ps1` builds the test image and runs the cases that declare Windows.

[Panoramic cases](../../docs/development/panoramic.md) explains the case format, the steps, how to run one case,
and how to read the results. [Testing](../../docs/development/testing.md) explains where integration tests fit
among the other tests.
