import contextlib
import hashlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import unittest.mock
from pathlib import Path


TOOLING = Path(__file__).parent
sys.path.insert(0, str(TOOLING))
spec = importlib.util.spec_from_file_location("install_macos_agent", TOOLING / "install_macos_agent.py")
subject = importlib.util.module_from_spec(spec)
spec.loader.exec_module(subject)

from test_agent_version import make_version_file  # noqa: E402


DMG_BYTES = b"test-only DMG contents"


def version_file_for(contents=DMG_BYTES):
    version_file = make_version_file()
    version_file["integration"]["macos"]["arm64"]["sha256"] = hashlib.sha256(contents).hexdigest()
    return version_file


def newer_package_version_file(contents):
    """Return a version file for another nightly build whose DMG holds `contents`."""
    version_file = version_file_for(contents)
    package = version_file["integration"]["macos"]["arm64"]
    package["url"] = package["url"].replace("pipeline.98765", "pipeline.98766")
    integration = version_file["integration"]
    integration["version"] = integration["version"].replace("pipeline.98765", "pipeline.98766")
    return version_file


class FakeInstaller:
    """Stands in for `hdiutil` and `pkgutil`, producing a payload with an Agent executable."""

    def __init__(self, attach_failures=0, executable=True):
        self.attach_failures = attach_failures
        self.executable = executable
        self.commands = []

    def __call__(self, command, **_):
        self.commands.append(command[0])
        if command[0] == "hdiutil":
            if self.attach_failures:
                self.attach_failures -= 1
                return subprocess.CompletedProcess(command, 1)
            (Path(command[command.index("-mountpoint") + 1]) / "datadog-agent.pkg").write_bytes(b"")
        elif command[0] == "pkgutil":
            agent = Path(command[3]) / "datadog-agent-core.pkg/Payload" / subject.AGENT_EXECUTABLE
            agent.parent.mkdir(parents=True)
            agent.write_text("#!/bin/sh\n", encoding="utf-8")
            agent.chmod(0o755 if self.executable else 0o644)
        return subprocess.CompletedProcess(command, 0)


class ProvisionTest(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name)
        self.install_dir = self.root / "sandbox/datadog-agent"
        self.cache_dir = self.root / "cache"
        self.downloads = []

    def fetch(self, contents=DMG_BYTES):
        def fetch(url, destination):
            self.downloads.append(url)
            Path(destination).write_bytes(contents)

        return fetch

    def provision(self, version_file=None, fetch=None, installer=None):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()), \
                unittest.mock.patch.object(subject.time, "sleep"):
            return subject.provision(
                version_file or version_file_for(), "arm64", self.install_dir, self.cache_dir, fetch or self.fetch(),
                installer or FakeInstaller(), self.root,
            )

    def marker(self):
        return json.loads((self.install_dir / subject.MARKER_NAME).read_text(encoding="utf-8"))

    def test_installs_and_records_the_package(self):
        version_file = version_file_for()
        self.assertTrue(self.provision(version_file))
        self.assertTrue(os.access(self.install_dir / subject.AGENT_EXECUTABLE, os.X_OK))
        self.assertEqual(self.marker(), subject.expected_marker(version_file, "arm64"))

    def test_reuses_an_install_whose_marker_matches(self):
        self.provision()
        installer = FakeInstaller()
        self.assertFalse(self.provision(installer=installer))
        self.assertEqual(installer.commands, [])
        self.assertEqual(len(self.downloads), 1)

    def test_reinstalls_when_the_marker_is_stale(self):
        self.provision()
        newer = b"test-only newer DMG contents"
        self.assertTrue(self.provision(newer_package_version_file(newer), fetch=self.fetch(newer)))
        self.assertEqual(self.marker()["sha256"], hashlib.sha256(newer).hexdigest())

    def test_reinstalls_when_the_executable_is_missing(self):
        self.provision()
        (self.install_dir / subject.AGENT_EXECUTABLE).unlink()
        self.assertTrue(self.provision())

    def test_rejects_a_download_with_the_wrong_checksum(self):
        with self.assertRaisesRegex(subject.InstallError, "but the version file expects"):
            self.provision(fetch=self.fetch(b"tampered"))
        self.assertEqual(list(self.cache_dir.iterdir()), [])
        self.assertFalse(self.install_dir.exists())

    def test_rejects_and_removes_a_cached_package_with_the_wrong_checksum(self):
        self.cache_dir.mkdir()
        url = version_file_for()["integration"]["macos"]["arm64"]["url"]
        cached = self.cache_dir / url.rsplit("/", 1)[1]
        cached.write_bytes(b"corrupted")
        with self.assertRaisesRegex(subject.InstallError, "cached .* removed it"):
            self.provision()
        self.assertFalse(cached.exists())
        self.assertEqual(self.downloads, [])

    def test_verifies_a_cached_package_before_using_it(self):
        self.provision()
        (self.install_dir / subject.MARKER_NAME).unlink()
        self.assertTrue(self.provision())
        self.assertEqual(len(self.downloads), 1)

    def test_a_failed_install_leaves_no_marker(self):
        self.provision()
        newer = b"test-only newer DMG contents"
        with self.assertRaisesRegex(subject.InstallError, "no executable"):
            self.provision(newer_package_version_file(newer), self.fetch(newer), FakeInstaller(executable=False))
        self.assertFalse((self.install_dir / subject.MARKER_NAME).exists())

        with self.assertRaisesRegex(subject.InstallError, "failed to attach"):
            self.provision(newer_package_version_file(newer), self.fetch(newer), FakeInstaller(attach_failures=3))
        self.assertFalse((self.install_dir / subject.MARKER_NAME).exists())


if __name__ == "__main__":
    unittest.main()
