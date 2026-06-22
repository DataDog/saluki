import random
import subprocess
from pathlib import Path

NUM_SAMPLES = 10

SALUKI_ROOT = Path.home() / "dd/saluki"
GRAMMAR = SALUKI_ROOT / "fuzz/dogstatsd_multi_offset.ebnf"
folder = SALUKI_ROOT/"fuzz/corpus/apd"
files = list(folder.iterdir())
sample = random.sample(files, min(NUM_SAMPLES, len(files)))

BARKUS_CLI = Path.home() / "dd/barkus/target/debug/barkus-cli"

for path in sample:
    hex_data = path.read_bytes().hex().encode()
    print(f"--- {path.name} ---")
    result = subprocess.run(
        [BARKUS_CLI.as_posix(), "decode", "--tape", "-", GRAMMAR.as_posix()],
        input=hex_data,
        capture_output=True,
    )
    print(result.stdout.decode("utf-8", errors="replace").strip())
    print()
