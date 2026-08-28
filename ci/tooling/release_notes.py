#!/usr/bin/env python3
"""Render and merge curated Saluki release notes."""

import argparse
import os
import re
from pathlib import Path


RELEASE_TAG_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
START_MARKER = "<!-- saluki-curated-notes:start -->"
END_MARKER = "<!-- saluki-curated-notes:end -->"


def is_release_tag(version: str) -> bool:
    """Return whether a version is an official Saluki release tag."""
    return bool(RELEASE_TAG_RE.fullmatch(version))


def build_curated_block(markdown: str) -> str:
    """Wrap non-empty curated Markdown in stable release-body markers."""
    if not markdown.strip():
        return ""

    return "\n".join((START_MARKER, "# Release notes", "", markdown.strip(), END_MARKER))


def merge_release_body(existing: str, markdown: str) -> str:
    """Prepend or replace the marker-bounded curated section in a release body."""
    block = build_curated_block(markdown)
    if not block:
        return existing

    start = existing.find(START_MARKER)
    end = existing.find(END_MARKER)
    if (start == -1) != (end == -1) or (end != -1 and end < start):
        raise ValueError("release body contains malformed curated-release-note markers")

    if start != -1:
        end += len(END_MARKER)
        return existing[:start] + block + existing[end:]

    return block + "\n\n" + existing if existing else block


def write_github_output(name: str, value: str) -> None:
    """Write an output value when running inside a GitHub Actions step."""
    output_path = os.environ.get("GITHUB_OUTPUT")
    if output_path:
        with Path(output_path).open("a", encoding="utf-8") as output_file:
            output_file.write(f"{name}={value}\n")


def merge_command(arguments: argparse.Namespace) -> int:
    """Merge a rendered note file into a release-body file."""
    markdown = arguments.notes_file.read_text(encoding="utf-8")
    existing = arguments.release_body_file.read_text(encoding="utf-8")
    merged = merge_release_body(existing, markdown)
    arguments.output.write_text(merged, encoding="utf-8")
    write_github_output("has_notes", str(bool(markdown.strip())).lower())
    return 0


def parse_arguments() -> argparse.Namespace:
    """Parse the release-note helper command line."""
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)

    merge_parser = subcommands.add_parser("merge", help="Merge curated notes into a GitHub release body")
    merge_parser.add_argument("--notes-file", type=Path, required=True)
    merge_parser.add_argument("--release-body-file", type=Path, required=True)
    merge_parser.add_argument("--output", type=Path, required=True)
    merge_parser.set_defaults(handler=merge_command)

    subcommands.add_parser("check", help="Validate Reno release-note files")
    subcommands.add_parser("render", help="Render a tag's Reno release notes")
    return parser.parse_args()


def main() -> int:
    """Run the requested release-note command."""
    arguments = parse_arguments()
    if not hasattr(arguments, "handler"):
        raise NotImplementedError(f"the {arguments.command!r} command is not implemented")
    return arguments.handler(arguments)


if __name__ == "__main__":
    raise SystemExit(main())
