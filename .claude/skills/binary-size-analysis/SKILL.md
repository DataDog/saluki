---
name: binary-size-analysis
description: >
  Analyze agent-data-plane binary size to identify top crates and modules contributing to binary
  size, and evaluate optimization opportunities. TRIGGER when: user asks about binary size, code
  size, shrinking the binary, binary optimization, reducing binary footprint, evaluating what
  contributes to binary size, or asks to find ways to make the binary smaller. DO NOT TRIGGER when:
  user asks about runtime memory usage, memory leaks, allocation profiling, or performance tuning.
---

# Binary Size Analysis for `agent-data-plane`

You are performing a standalone binary size analysis of the `agent-data-plane` binary. Your goal is
to identify the top 3-5 contributors to binary size and assess whether each can feasibly be reduced.

## Step 1: Build with optimized-release profile

Build the binary using the production release profile. Warn the user this may take several minutes
due to fat LTO and single codegen unit.

```bash
cargo build --profile optimized-release -p agent-data-plane
```

The `optimized-release` profile is defined in the workspace root `Cargo.toml` and inherits from
`release` with `lto = "fat"` and `codegen-units = 1`. This matches what ships to users.

## Step 2: Separate debug info and strip the binary

Create a separate debug file and strip the binary, so we analyze the actual shipped artifact size
while retaining symbols for attribution. This follows the established CI pattern from
`.gitlab/benchmark.yml`.

```bash
cp target/optimized-release/agent-data-plane target/optimized-release/agent-data-plane.analyzed
objcopy --only-keep-debug target/optimized-release/agent-data-plane.analyzed target/optimized-release/agent-data-plane.debug
strip --strip-debug target/optimized-release/agent-data-plane.analyzed
```

We work on a copy (`.analyzed`) so we don't destroy the original build artifact. Report the stripped
binary size to the user as the headline number.

## Step 3: Multi-tool analysis

Run these analyses to build a comprehensive picture.

**IMPORTANT — Bloaty DWARF attribution is unreliable with fat LTO + single CGU.** Bloaty uses DWARF
debug info ranges to attribute sizes to symbols. With fat LTO and `codegen-units = 1`, these DWARF
ranges become wildly inaccurate — a small function's debug range can span a huge address range that
actually belongs to other functions. In testing, bloaty reported a 21 KiB function as 1.79 MiB
(88x inflation), and multiple symbols showed 5-176x inflation. Large async state machine functions
(>25 KiB) tended to be accurate within ~20%, but smaller functions were often grossly inflated.

**Use ELF symbol sizes as ground truth for per-symbol sizes.** The ELF symbol table (`readelf -Ws`
or `nm --size-sort`) records the actual function sizes set by the compiler/linker and is not affected
by DWARF range issues. Always cross-reference bloaty's per-symbol claims against `nm`.

**`cargo bloat --crates` is the most trustworthy crate-level view.** It uses ELF symbol sizes (not
DWARF), so it is not affected by the LTO attribution problem.

Bloaty's **section breakdown** (`-d sections`) remains accurate since it reads ELF section headers
directly, not DWARF. Bloaty's **compile unit** and **symbol** breakdowns should be treated as
directional hints only — always validate specific claims against `nm`/`readelf`.

### 3a. cargo bloat — crate-level attribution (primary source of truth)

```bash
cargo bloat --profile optimized-release -p agent-data-plane --crates -n 20
```

This gives the most trustworthy crate-level view of `.text` section contributions, using ELF symbol
sizes. For the top crates, drill in:

```bash
cargo bloat --profile optimized-release -p agent-data-plane --filter <crate_name> -n 20
```

### 3b. Largest symbols via nm (ground truth for per-symbol sizes)

Use `nm` on the **unstripped** binary (not the `.analyzed` copy) for accurate ELF symbol sizes:

```bash
nm --size-sort --reverse-sort target/optimized-release/agent-data-plane | rustfilt | head -60
```

The first column is the symbol size in hex. This is the authoritative size for each symbol.

### 3c. Bloaty — section breakdown (accurate)

```bash
bloaty -d sections \
  --debug-file=target/optimized-release/agent-data-plane.debug \
  target/optimized-release/agent-data-plane.analyzed
```

This reveals the proportion of `.text` (code), `.rodata` (read-only data), `.data`, `.bss`, etc.
Section-level data comes from ELF headers and is reliable.

### 3d. Bloaty — compile unit breakdown (directional only)

```bash
bloaty -d compileunits -n 30 \
  --debug-file=target/optimized-release/agent-data-plane.debug \
  target/optimized-release/agent-data-plane.analyzed
```

This shows which source files / compile units contribute the most code. **Treat as directional** —
with fat LTO, compile unit attribution can be imprecise.

### 3e. Bloaty — symbol breakdown (cross-reference with nm)

```bash
bloaty -d symbols -n 50 \
  --debug-file=target/optimized-release/agent-data-plane.debug \
  target/optimized-release/agent-data-plane.analyzed | rustfilt
```

**Always cross-reference the top symbols against `nm --size-sort` to validate.** If bloaty reports a
symbol as significantly larger than `nm` does (>1.5x), trust `nm`. You can batch-validate like this:

```bash
# Get bloaty's CSV output
bloaty -d symbols -n 50 --csv \
  --debug-file=target/optimized-release/agent-data-plane.debug \
  target/optimized-release/agent-data-plane.analyzed > /tmp/bloaty_symbols.csv

# Compare against nm for the same symbols (match by hash suffix)
nm --size-sort --reverse-sort target/optimized-release/agent-data-plane | rustfilt > /tmp/nm_symbols.txt
```

Then compare the sizes for each symbol. Flag any where bloaty exceeds nm by >1.5x as a DWARF
attribution artifact — do not report those inflated sizes to the user.

### 3f. Dependency tree

```bash
cargo tree -p agent-data-plane -e normal --depth 1
```

For heavy crates identified in the analysis, investigate their transitive dependencies:

```bash
cargo tree -p agent-data-plane -i <crate_name>
```

## Step 4: Investigate top contributors

For each of the top 3-5 contributors identified above, perform targeted investigation.

### Distinguishing workspace vs third-party crates

Use `cargo metadata --no-deps --format-version 1` to get the workspace crate list (packages with
`source: null`). The existing script at `test/one-off/analyze-binary-size.py` has this logic as a
reference. Workspace crates use underscores in symbols but hyphens in `Cargo.toml`.

### For third-party dependencies

- Check if heavy transitive deps are being pulled in: `cargo tree -i <crate>`
- Check if unnecessary Cargo features are enabled: `cargo tree -p agent-data-plane -f '{p} {f}' | grep <crate>`
- Research whether a lighter alternative crate exists
- Check if feature flags could reduce scope (for example, disabling default features)

### For workspace crates

- Examine whether size comes from **monomorphized generics**: look for many instantiations of the
  same function with different type parameters in the symbol list. A tell-tale sign is seeing the
  same function name repeated with different type suffixes.
- Check for **large inline code** or **embedded data** (for example, HTML templates, lookup tables).
- For monomorphization issues: evaluate whether type-erasure (`dyn Trait`, `Box<dyn ...>`) or
  consolidating generic instantiations could reduce code size. Reference the recent work on
  `saluki-io` where `HttpClient` was refactored to use type-erased `ClientBody` to reduce
  monomorphization of HTTP client/server types.

### Monomorphization deep-dive

When you suspect monomorphization is a significant contributor for a crate, use `nm` (not bloaty) to
get accurate symbol sizes and grep for that crate's symbols:

```bash
nm --size-sort --reverse-sort target/optimized-release/agent-data-plane | rustfilt | grep '<crate_name>' | head -30
```

The first column is the hex size. Look for the same function appearing with different generic
instantiations (different hash suffixes but same base name). Count unique instantiations vs unique
base function names to quantify the monomorphization overhead.

## Step 5: Present findings

Produce a summary table like:

| Rank | Crate/Module | Size | Category | Feasibility | Notes |
|------|-------------|------|----------|-------------|-------|
| 1 | `example_crate` | ~1.5 MiB | Third-party dep | Medium | Could disable unused features X, Y |
| 2 | `saluki_io::net` | ~800 KiB | Monomorphization | Easy | 12 instantiations of `send_request<T>`, type-erasure viable |
| ... | | | | | |

Categories: `third-party dep`, `workspace code`, `monomorphization`, `embedded data`, `protocol buffers`, `serialization/deserialization`

Feasibility: `easy` / `medium` / `hard` with a one-line rationale.

After presenting the table, ask the user:

> "Would you like me to create a plan to tackle any of these specific areas?"

Do NOT proceed to implementation without user direction. The goal of this skill is **diagnosis and
feasibility assessment**, not automatic refactoring.
