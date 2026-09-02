#!/usr/bin/env python3
"""Render and merge curated Saluki release notes."""

import argparse
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

import yaml


RELEASE_TAG_RE = re.compile(r"^[0-9]+\.[0-9]+\.[0-9]+$")
START_MARKER = "<!-- saluki-curated-notes:start -->"
END_MARKER = "<!-- saluki-curated-notes:end -->"
CATEGORY_ORDER = ("upgrade", "features", "enhancements", "issues", "deprecations", "security", "fixes", "other")
RENO_FILENAME_RE = re.compile(r"^.+-([0-9a-f]{16})\.yaml$")
# Reno renders note prose as reStructuredText, so Markdown markup survives into the release body
# verbatim. Each pattern pairs with a description of the reStructuredText spelling to use instead.
MARKDOWN_PATTERNS = (
    (re.compile(r"!\[[^\]]*\]\(([^)]+)\)"), "image syntax; use a '.. image:: {0}' directive"),
    (re.compile(r"(?<!!)\[([^\]]+)\]\(([^)]+)\)"), "link syntax; use '`{0} <{1}>`_'"),
    (re.compile(r"__([^_]+)__"), "bold syntax; use '**{0}**'"),
    # A single-backtick span is Markdown inline code, unless it is a reStructuredText hyperlink or
    # reference (both end in an underscore) or the payload of an explicit role such as :code:`x`.
    (re.compile(r"(?<![:`])`([^`]+)`(?![_`])"), "single-backtick inline code; use '``{0}``'"),
    (re.compile(r"^#{1,6}\s+\S"), "heading syntax; use a reStructuredText title underline"),
    (re.compile(r"^```"), "fenced code block; use a '.. code-block::' directive"),
    (re.compile(r"^>\s+\S"), "block quote syntax; use indentation or a '.. note::' directive"),
)
# Prose inside a reStructuredText inline literal renders verbatim, so Markdown spelled there is
# intentional rather than a mistake.
RST_INLINE_LITERAL_RE = re.compile(r"``[^`]+``")
RELEASE_NOTE_CONFIG_PATH = "releasenotes/config.yaml"


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


def find_markdown_syntax(text: str) -> list[str]:
    """Return descriptions of the Markdown constructs used in one release-note entry."""
    return [
        description.format(*match.groups())
        for line in (RST_INLINE_LITERAL_RE.sub(" ", line) for line in text.splitlines())
        for pattern, description in MARKDOWN_PATTERNS
        for match in pattern.finditer(line)
    ]


def find_duplicate_note_ids(paths: list[Path]) -> list[str]:
    """Return errors for release notes sharing a Reno unique identifier."""
    notes_by_id: dict[str, list[Path]] = {}
    for path in sorted(paths):
        match = RENO_FILENAME_RE.fullmatch(path.name)
        if match:
            notes_by_id.setdefault(match.group(1), []).append(path)
    return [
        f"{', '.join(str(path) for path in notes)}: release notes share the Reno unique identifier "
        f"{identifier!r}; create notes with `reno new` so each one gets a fresh identifier"
        for identifier, notes in sorted(notes_by_id.items())
        if len(notes) > 1
    ]


def validate_note_file(path: Path) -> list[str]:
    """Return validation errors for one opt-in Reno release note."""
    errors = []
    if not RENO_FILENAME_RE.fullmatch(path.name):
        errors.append(f"{path}: filename must end in -<16 lowercase hex characters>.yaml")
    try:
        content = yaml.safe_load(path.read_text(encoding="utf-8"))
    except yaml.YAMLError as error:
        return [*errors, f"{path}: invalid YAML: {error}"]
    if not isinstance(content, dict) or not content:
        return [*errors, f"{path}: release note must be a non-empty YAML mapping"]
    for category, entries in content.items():
        if category not in CATEGORY_ORDER:
            errors.append(f"{path}: unsupported release-note category {category!r}")
        elif not isinstance(entries, list) or not entries or any(not isinstance(entry, str) or not entry.strip() for entry in entries):
            errors.append(f"{path}: {category!r} must be a non-empty list of non-empty strings")
        else:
            errors.extend(
                f"{path}: {category!r} uses Markdown {problem}"
                for entry in entries
                for problem in find_markdown_syntax(entry)
            )
    return errors


def check_command(arguments: argparse.Namespace) -> int:
    """Validate every supplied release-note file."""
    errors = [error for path in arguments.note_files for error in validate_note_file(path)]
    errors.extend(find_duplicate_note_ids(arguments.note_files))
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    return 0


def get_error_detail(error: subprocess.CalledProcessError) -> str:
    """Return a subprocess failure's diagnostic text."""
    return (error.stderr or "").strip() or str(error)


def is_empty_reno_report(error: subprocess.CalledProcessError, version: str) -> bool:
    """Return whether Reno reported that the requested version has no notes."""
    return bool(re.search(rf"KeyError: ['\"]{re.escape(version)}['\"]\s*$", error.stderr or ""))


def render_release_notes(version: str, repository: Path) -> str:
    """Render one tagged release's Reno notes as GitHub-flavored Markdown."""
    if not is_release_tag(version):
        raise ValueError("release version must use the X.Y.Z format")

    def run(command: list[str], **kwargs) -> subprocess.CompletedProcess[str]:
        return subprocess.run(command, cwd=repository, check=True, capture_output=True, text=True, **kwargs)

    try:
        run(["git", "rev-parse", "--verify", f"refs/tags/{version}"])
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"could not verify release tag {version}: {get_error_detail(error)}") from error

    try:
        run(["git", "cat-file", "-e", f"refs/tags/{version}:{RELEASE_NOTE_CONFIG_PATH}"])
    except subprocess.CalledProcessError as error:
        raise RuntimeError(
            f"release tag {version} does not contain {RELEASE_NOTE_CONFIG_PATH}; "
            "curated release notes cannot be repaired before their adoption"
        ) from error

    reno = shutil.which("reno") or str(Path(sys.executable).with_name("reno"))
    try:
        rst = run(
            [
                reno,
                "report",
                "--ignore-cache",
                "--no-show-source",
                "--version",
                version,
                "--branch",
                version,
            ]
        ).stdout
    except subprocess.CalledProcessError as error:
        if is_empty_reno_report(error, version):
            return ""
        raise RuntimeError(f"could not render release notes for {version}: {get_error_detail(error)}") from error

    if not rst.strip():
        return ""

    try:
        return run(["pandoc", "--from", "rst", "--to", "gfm", "--wrap=none"], input=rst).stdout
    except subprocess.CalledProcessError as error:
        raise RuntimeError(f"could not convert release notes for {version}: {get_error_detail(error)}") from error


def render_command(arguments: argparse.Namespace) -> int:
    """Render a tag's notes to a file or standard output."""
    rendered = render_release_notes(arguments.version, arguments.repository)
    if str(arguments.output) == "-":
        print(rendered, end="")
    else:
        arguments.output.write_text(rendered, encoding="utf-8")
    write_github_output("has_notes", str(bool(rendered.strip())).lower())
    return 0


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

    check_parser = subcommands.add_parser("check", help="Validate Reno release-note files")
    check_parser.add_argument("note_files", nargs="*", type=Path)
    check_parser.set_defaults(handler=check_command)
    render_parser = subcommands.add_parser("render", help="Render a tag's Reno release notes")
    render_parser.add_argument("--version", required=True)
    render_parser.add_argument("--repository", type=Path, default=Path.cwd())
    render_parser.add_argument("--output", type=Path, required=True)
    render_parser.set_defaults(handler=render_command)
    return parser.parse_args()


def main() -> int:
    """Run the requested release-note command."""
    arguments = parse_arguments()
    if not hasattr(arguments, "handler"):
        raise NotImplementedError(f"the {arguments.command!r} command is not implemented")
    try:
        return arguments.handler(arguments)
    except (RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
