#!/usr/bin/env python3
"""Install the macOS Datadog Agent named in `.datadog-agent-version.json` into a sandbox directory.

This sets up the Agent for the macOS integration tests, in CI and locally, through `make provision-macos-test-env`.
It isn't part of the workflows that update the version file.

The DMG for the host architecture is verified against its recorded SHA-256 before it is mounted, whether it was just
downloaded or found in the cache. A marker recording the version, URL and checksum is written only after a successful
install, and an installation is reused only when its marker matches the version file and the Agent executable exists.
"""

import argparse
import hashlib
import json
import os
import platform
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path, PurePosixPath
from urllib.parse import urlparse

sys.path.insert(0, str(Path(__file__).resolve().parent))
import agent_version  # noqa: E402


MARKER_NAME = ".saluki-agent-version.json"
AGENT_EXECUTABLE = Path("bin/agent/agent")
CLEANUP_SCRIPT = Path(__file__).resolve().with_name("cleanup-macos-dda-mounts.sh")


class InstallError(Exception):
    """The macOS Agent could not be fetched, verified or installed."""


def sha256_file(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as file:
        for chunk in iter(lambda: file.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def download(url, destination):
    """Download `url` to `destination`, failing on any HTTP error."""
    result = subprocess.run(["curl", "-fL", "--retry", "3", "--connect-timeout", "30", "-o", str(destination), url])
    if result.returncode != 0:
        raise InstallError(f"could not download {url} (curl exit code {result.returncode})")


def fetch_verified(url, sha256, cache_dir, fetch=download):
    """Return a cached copy of `url` whose SHA-256 matches the version file.

    Only verified downloads enter the cache, so a cached file that does not match was changed after download. It is
    deleted and the run fails rather than trusting it.
    """
    cache_dir = Path(cache_dir)
    cache_dir.mkdir(parents=True, exist_ok=True)
    path = cache_dir / PurePosixPath(urlparse(url).path).name

    if path.exists():
        actual = sha256_file(path)
        if actual != sha256:
            path.unlink()
            raise InstallError(f"cached {path} has SHA-256 {actual}, but the version file expects {sha256}; removed it")
        print(f"[*] Using cached {path} (SHA-256 verified)")
        return path

    descriptor, partial = tempfile.mkstemp(prefix=f".{path.name}.", dir=cache_dir)
    os.close(descriptor)
    try:
        print(f"[*] Downloading {url}...")
        fetch(url, partial)
        actual = sha256_file(partial)
        if actual != sha256:
            raise InstallError(f"{url} has SHA-256 {actual}, but the version file expects {sha256}")
        os.replace(partial, path)
    finally:
        Path(partial).unlink(missing_ok=True)
    return path


def expected_marker(version_file, arch):
    package = version_file["integration"]["macos"][arch]
    return {"version": version_file["integration"]["version"], "url": package["url"], "sha256": package["sha256"]}


def is_installed(install_dir, marker):
    """Return whether `install_dir` holds a complete install of the Agent in the version file."""
    marker_path = Path(install_dir) / MARKER_NAME
    executable = Path(install_dir) / AGENT_EXECUTABLE
    try:
        recorded = json.loads(marker_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False
    return recorded == marker and executable.is_file() and os.access(executable, os.X_OK)


def install(dmg, install_dir, marker, run=subprocess.run, mount_root="/tmp"):
    """Extract the Agent payload from `dmg` into `install_dir`, then write the marker."""
    install_dir = Path(install_dir)
    (install_dir / MARKER_NAME).unlink(missing_ok=True)

    # The `saluki-dda-mount-` prefix lets cleanup-macos-dda-mounts.sh find mounts that an interrupted run left behind.
    mount_dir = tempfile.mkdtemp(prefix="saluki-dda-mount-", dir=mount_root)
    expand_dir = Path(tempfile.mkdtemp(prefix="saluki-dda-expand-", dir=mount_root))
    expand_dir.rmdir()
    try:
        try:
            for attempt in range(1, 4):
                attached = run(["hdiutil", "attach", str(dmg), "-mountpoint", mount_dir, "-nobrowse"],
                               stdout=subprocess.DEVNULL)
                if attached.returncode == 0:
                    break
                if attempt == 3:
                    raise InstallError(f"failed to attach {dmg} after {attempt} attempts")
                print(f"[*] Failed to attach {dmg} on attempt {attempt}; retrying...", file=sys.stderr)
                time.sleep(attempt * 2)

            packages = sorted(Path(mount_dir).rglob("*.pkg"))
            if not packages:
                raise InstallError(f"{dmg} contains no .pkg installer")
            expanded = run(["pkgutil", "--expand-full", str(packages[0]), str(expand_dir)], stdout=subprocess.DEVNULL)
            if expanded.returncode != 0:
                raise InstallError(f"pkgutil could not expand {packages[0]}")
        finally:
            run(["bash", str(CLEANUP_SCRIPT), mount_dir])

        payloads = [path for path in expand_dir.rglob("Payload") if path.is_dir()]
        if not payloads or not os.access(payloads[0] / AGENT_EXECUTABLE, os.X_OK):
            raise InstallError(f"the package payload has no executable {AGENT_EXECUTABLE}")

        shutil.rmtree(install_dir, ignore_errors=True)
        install_dir.parent.mkdir(parents=True, exist_ok=True)
        shutil.move(str(payloads[0]), str(install_dir))
    finally:
        shutil.rmtree(expand_dir, ignore_errors=True)

    (install_dir / MARKER_NAME).write_text(json.dumps(marker, indent=2) + "\n", encoding="utf-8")


def provision(version_file, arch, install_dir, cache_dir, fetch=download, run=subprocess.run, mount_root="/tmp"):
    """Install the Agent in the version file for `arch` unless a matching install already exists."""
    marker = expected_marker(version_file, arch)
    print(f"[*] Agent {marker['version']}: {marker['url']} (SHA-256 {marker['sha256']})")
    if is_installed(install_dir, marker):
        print(f"[*] Datadog Agent {marker['version']} is already installed in {install_dir}")
        return False
    dmg = fetch_verified(marker["url"], marker["sha256"], cache_dir, fetch)
    print(f"[*] Installing Datadog Agent {marker['version']} into {install_dir}...")
    install(dmg, install_dir, marker, run, mount_root)
    return True


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--file", default=agent_version.VERSION_FILE)
    parser.add_argument("--install-dir", required=True)
    parser.add_argument("--cache-dir", required=True)
    arguments = parser.parse_args(argv)

    arch = platform.machine()
    if arch not in agent_version.MACOS_ARCHITECTURES:
        print(f"error: no macOS package in the version file for architecture {arch!r}", file=sys.stderr)
        return 1
    try:
        version_file = agent_version.load_version_file(arguments.file)
        provision(version_file, arch, Path(arguments.install_dir), Path(arguments.cache_dir))
    except (agent_version.VersionFileError, InstallError, OSError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
