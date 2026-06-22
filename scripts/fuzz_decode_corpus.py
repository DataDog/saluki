import argparse
import random
import subprocess
from pathlib import Path

NUM_SAMPLES = 10

SALUKI_ROOT = Path.home() / "dd/saluki"
GRAMMAR = SALUKI_ROOT / "fuzz/dogstatsd_multi_offset.ebnf"
folder = SALUKI_ROOT/"fuzz/corpus/apd"

BARKUS_CLI = Path.home() / "dd/barkus/target/debug/barkus-cli"


def decode(path):
    hex_data = path.read_bytes().hex().encode()
    print(f"--- {path.name} ---")
    result = subprocess.run(
        [BARKUS_CLI.as_posix(), "decode", "--tape", "-", GRAMMAR.as_posix()],
        input=hex_data,
        capture_output=True,
    )
    print(result.stdout.decode("utf-8", errors="replace").strip())
    print()


def main():
    parser = argparse.ArgumentParser(description="Decode dogstatsd fuzz corpus files with barkus-cli")
    parser.add_argument("file", nargs="?", default=None, help="decode only this file instead of sampling")
    args = parser.parse_args()

    if args.file is not None:
        paths = [Path(args.file)]
    else:
        files = list(folder.iterdir())
        paths = random.sample(files, min(NUM_SAMPLES, len(files)))

    for path in paths:
        decode(path)


if __name__ == "__main__":
    main()
