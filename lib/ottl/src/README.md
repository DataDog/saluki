# ottl — Rust OTTL Parser Library

**ottl** is a Rust library for parsing and executing [OpenTelemetry Transformation Language (OTTL)](https://github.com/open-telemetry/opentelemetry-collector-contrib/tree/main/pkg/ottl) expressions.

## Table of Contents

- [Overview](#overview)
- [Architecture](#architecture)
- [Data Types](#data-types)
- [API](#api)
- [Usage Examples](#usage-examples)
- [OTTL Grammar](#ottl-grammar)
- [Modules](#modules)
- [Performance](#performance)

---

## Overview

The library provides a complete pipeline for working with OTTL:

1. **Lexical Analysis** — tokenization of the input string (`lexer` module)
2. **Syntax Analysis** — AST construction with callback binding at parse time (`parser` module)
3. **Execution** — AST interpretation with support for conditions, mathematical and logical expressions

### Key Features

- Callback binding (editors/converters) at parse time, not execution time
- Full OTTL grammar support: literals, paths, lists, maps, functions
- Mathematical expressions with operator precedence
- Logical expressions with short-circuit evaluation
- Extensible path system via `PathResolver`

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        Parser::new()                        │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────┐    ┌──────────┐    ┌─────────────────────────┐ │
│  │  Lexer  │───▶│  Parser  │───▶│  AST + Bound Callbacks  │ │
│  │ (logos) │    │(chumsky) │    │                         │ │
│  └─────────┘    └──────────┘    └─────────────────────────┘ │
├─────────────────────────────────────────────────────────────┤
│                      Parser::execute()                      │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────────────────────────────────────────────────────┐│
│  │              AST Evaluation Engine                      ││
│  │  • evaluate_root()                                      ││
│  │  • evaluate_bool_expr()                                 ││
│  │  • evaluate_math_expr()                                 ││
│  │  • evaluate_function_call()                             ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
```

---

## Data Types

### Value

Universal type for all values in OTTL:

```rust
pub enum Value {
    Nil,                        // null/nil
    Bool(bool),                 // true, false
    Int(i64),                   // 42, -10
    Float(f64),                 // 3.14, .5
    String(String),             // "hello"
    Bytes(Vec<u8>),             // 0xDEADBEEF
    List(Vec<Value>),           // [1, 2, 3]
    Map(HashMap<String, Value>) // {"key": "value"}
}
```

### Argument

Function arguments (positional and named):

```rust
pub enum Argument {
    Positional(Value),                      // set(path, "value")
    Named { name: String, value: Value }    // set(target=path, value="x")
}
```

### Callback Types

```rust
// Callback function for editors and converters
pub type CallbackFn = Arc<dyn Fn(&mut EvalContext, Vec<Argument>) -> Result<Value> + Send + Sync>;

// Map of function names → their implementations
pub type CallbackMap = HashMap<String, CallbackFn>;

// Map of enum values → their numeric equivalents
pub type EnumMap = HashMap<String, i64>;

// Path resolver (e.g., "resource.attributes")
pub type PathResolver = Arc<dyn Fn(&str) -> Result<Arc<dyn PathAccessor + Send + Sync>> + Send + Sync>;
```

### PathAccessor

Trait for accessing values by path:

```rust
pub trait PathAccessor: fmt::Debug {
    fn get(&self, ctx: &EvalContext, path: &str) -> Result<&Value>;
    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> Result<()>;
}
```

---

## API

### Creating a Parser

```rust
use ottl::{Parser, OttlParser, CallbackMap, EnumMap, PathResolver};

let editors: CallbackMap = HashMap::new();
let converters: CallbackMap = HashMap::new();
let enums: EnumMap = HashMap::new();
let resolver: PathResolver = Arc::new(|path| { /* ... */ });

let parser = Parser::new(
    &editors,
    &converters,
    &enums,
    &resolver,
    "set(resource.attributes[\"key\"], \"value\") where status == 200"
);
```

### Checking for Parse Errors

```rust
if let Err(e) = parser.is_error() {
    eprintln!("Parse error: {}", e);
    return;
}
```

### Execution

```rust
let mut ctx: EvalContext = Box::new(MyContext::new());
let result = parser.execute(&mut ctx)?;
```

### OttlParser Trait

```rust
pub trait OttlParser {
    /// Checks for parsing errors
    fn is_error(&self) -> Result<()>;
    
    /// Executes the OTTL expression
    fn execute(&self, ctx: &mut EvalContext) -> Result<Value>;
}
```

---

## Usage Examples

### Editor with WHERE Condition

```rust
// OTTL: set(body, "modified") where status_code >= 400

let mut editors = CallbackMap::new();
editors.insert("set".to_string(), Arc::new(|ctx, args| {
    // args[0] = path, args[1] = value
    // Value setting logic...
    Ok(Value::Nil)
}));
```

### Converter

```rust
// OTTL: Concat(first_name, " ", last_name)

let mut converters = CallbackMap::new();
converters.insert("Concat".to_string(), Arc::new(|ctx, args| {
    let mut result = String::new();
    for arg in args {
        if let Value::String(s) = arg.value() {
            result.push_str(s);
        }
    }
    Ok(Value::String(result))
}));
```

### Enum

```rust
// OTTL: status == HTTP_OK

let mut enums = EnumMap::new();
enums.insert("HTTP_OK".to_string(), 200);
enums.insert("HTTP_NOT_FOUND".to_string(), 404);
```

### Mathematical Expressions

```ottl
// Supported operators: +, -, *, /
// Precedence: * / higher than + -

result * 2 + offset
(a + b) / count
-value + 10
```

### Logical Expressions

```ottl
// Operators: and, or, not
// Comparison: ==, !=, <, >, <=, >=

status >= 200 and status < 300
not is_error or severity == "critical"
(type == "http" or type == "grpc") and latency > 100
```

---

## OTTL Grammar

### Root Expression

```ebnf
ROOT = EDITOR_INVOCATION_STATEMENT | BOOLEAN_EXPRESSION | MATH_EXPRESSION
```

### Editor Statement

```ebnf
EDITOR_INVOCATION_STATEMENT = EDITOR_INVOCATION [WHERE_CLAUSE]
WHERE_CLAUSE = "where" BOOLEAN_EXPRESSION
EDITOR_INVOCATION = lower_ident "(" [ARG_LIST] ")"
```

### Values

```ebnf
VALUE = PATH | LIST | MAP | LITERAL | ENUM | CONVERTER_INVOCATION | MATH_EXPRESSION

PATH = lower_ident {"." ident_segment} {INDEX}
LIST = "[" [VALUE {"," VALUE}] "]"
MAP = "{" [STRING_LITERAL ":" VALUE {"," STRING_LITERAL ":" VALUE}] "}"
LITERAL = STRING | INT | FLOAT | BYTES | BOOL | NIL

INDEX = "[" (STRING_LITERAL | INT_LITERAL) "]"
```

### Literal Types

| Type | Examples |
|------|----------|
| String | `"hello"`, `"with \"escapes\""` |
| Int | `42`, `-10`, `0` |
| Float | `3.14`, `.5`, `-2.0` |
| Bytes | `0xDEADBEEF`, `0x00` |
| Bool | `true`, `false` |
| Nil | `nil` |

### Identifiers

- **Lowercase** (`lower_ident`): starts with `a-z`, used for editors and paths
- **Uppercase** (`UPPER_IDENT`): starts with `A-Z`, used for converters and enums

---

## Modules

### `lexer.rs`

Lexical analyzer based on the [logos](https://crates.io/crates/logos) library.

**Main Components:**

- `Token<'a>` — enum of all OTTL tokens
- `Lexer` — wrapper over logos lexer
- `LexerError` — error with position information

**Tokens:**

| Category | Tokens |
|----------|--------|
| Keywords | `where`, `or`, `and`, `not`, `true`, `false`, `nil` |
| Comparison | `==`, `!=`, `<`, `>`, `<=`, `>=` |
| Arithmetic | `+`, `-`, `*`, `/` |
| Delimiters | `(`, `)`, `[`, `]`, `{`, `}`, `,`, `.`, `:`, `=` |
| Literals | `StringLiteral`, `IntLiteral`, `FloatLiteral`, `BytesLiteral` |
| Identifiers | `LowerIdent`, `UpperIdent` |

### `parser.rs`

Syntax analyzer based on [chumsky 0.12](https://crates.io/crates/chumsky).

**AST Types:**

```rust
// Root expressions
enum RootExpr {
    EditorStatement(EditorStatement),
    BooleanExpression(BoolExpr),
    MathExpression(MathExpr),
}

// Editor statement
struct EditorStatement {
    editor: FunctionCall,
    condition: Option<BoolExpr>,
}

// Function call
struct FunctionCall {
    name: String,
    is_editor: bool,
    args: Vec<ArgExpr>,
    indexes: Vec<IndexExpr>,
    callback: Option<CallbackFn>,  // Bound at parse time!
}

// Values
enum ValueExpr {
    Literal(Value),
    Path(PathExpr),
    List(Vec<ValueExpr>),
    Map(Vec<(String, ValueExpr)>),
    FunctionCall(Box<FunctionCall>),
    Math(Box<MathExpr>),
}

// Mathematical expressions
enum MathExpr {
    Primary(ValueExpr),
    Negate(Box<MathExpr>),
    Binary { left: Box<MathExpr>, op: MathOp, right: Box<MathExpr> },
}

// Logical expressions
enum BoolExpr {
    Literal(bool),
    Comparison { left: ValueExpr, op: CompOp, right: ValueExpr },
    Converter(Box<FunctionCall>),
    Path(PathExpr),
    Not(Box<BoolExpr>),
    And(Box<BoolExpr>, Box<BoolExpr>),
    Or(Box<BoolExpr>, Box<BoolExpr>),
}
```

### `mod.rs`

Main library module, exports the public API:

- `Parser` — main parser
- `OttlParser` — public API trait
- `Value`, `Argument` — data types
- `CallbackFn`, `CallbackMap`, `EnumMap` — callback types
- `PathAccessor`, `PathResolver` — path handling types
- `BoxError`, `Result` — error types

### `tests.rs`

Extensive test suite (~2300+ lines):

- Lexer tests (tokenization of all types)
- Parser tests (all OTTL constructs)
- Execution tests (editors, converters, conditions)
- Mathematical expression tests
- Logical expression tests
- Comparison tests for all types
- Edge cases and error handling

---

## Performance

The library includes a comprehensive benchmark suite to measure execution performance. Benchmarks are implemented as ignored tests and can be run on demand.

### Running Benchmarks

```bash
# Run all benchmarks
cargo test bench_ -- --ignored --nocapture

# Run a specific benchmark
cargo test bench_execute_math_simple -- --ignored --nocapture
cargo test bench_parser_creation -- --ignored --nocapture
```

### Benchmark Suite

| Benchmark | Description | Expression |
|-----------|-------------|------------|
| `bench_execute_math_simple` | Simple arithmetic | `1 + 2 * 3 - 4 / 2` |
| `bench_execute_math_complex` | Nested parentheses | `((1 + 2) * (3 + 4)) / ((5 - 2) * (6 - 4)) + (10 * (2 + 3))` |
| `bench_execute_bool_comparisons` | Logical operations | `(1 < 2) and (3 > 1) or (5 == 5) and not (10 < 5)` |
| `bench_execute_with_paths` | Path resolution | `my.int.value + other.int.count * 2 - third.float.value` |
| `bench_execute_bool_with_paths` | Boolean with paths | `my.bool.enabled and (my.int.status > 0) or (other.int.count < 100)` |
| `bench_execute_with_converters` | Converter calls | `Add(1, 2) + Add(3, 4) * Add(5, 6)` |
| `bench_execute_complex_realistic` | Real-world scenario | `set(my.int.value, 100) where (my.int.status == STATUS_OK or ...) and my.bool.enabled` |
| `bench_parser_creation` | Parse time (not execute) | Complex math expression |

### Benchmark Results

Results from running on Apple M1 Pro (10 cores), 100,000 iterations per benchmark:

| Benchmark | Avg | Median | P99 | Throughput |
|-----------|-----|--------|-----|------------|
| **math_simple** | 83 ns | 83 ns | 125 ns | ~12M ops/sec |
| **math_complex** | 292 ns | 292 ns | 375 ns | ~3.4M ops/sec |
| **bool_comparisons** | 142 ns | 125 ns | 250 ns | ~7M ops/sec |
| **with_paths** | 625 ns | 584 ns | 917 ns | ~1.6M ops/sec |
| **bool_with_paths** | 500 ns | 458 ns | 792 ns | ~2M ops/sec |
| **with_converters** | 542 ns | 500 ns | 834 ns | ~1.8M ops/sec |
| **complex_realistic** | 709 ns | 667 ns | 1042 ns | ~1.4M ops/sec |
| **parser_creation** | 42 µs | — | — | ~24K parses/sec |

### Benchmark Output Format

Each benchmark outputs detailed statistics:

```
========================================
BENCHMARK: math_simple
========================================
Iterations: 100000
Total time: 8.3ms
----------------------------------------
Min:       42 ns (  0.04 µs)
Max:     5125 ns (  5.13 µs)
Avg:       83 ns (  0.08 µs)
Median:    83 ns (  0.08 µs)
P99:      125 ns (  0.13 µs)
----------------------------------------
Throughput: 12048192 ops/sec
========================================
```

### Performance Characteristics

- **Pure math expressions**: ~80-300 ns per execution
- **Path resolution overhead**: ~400-500 ns additional (depends on PathAccessor implementation)
- **Converter calls**: ~100-200 ns per call (depends on callback complexity)
- **Parser creation**: ~40-50 µs per parse (one-time cost, parser is reusable)

### Optimization Notes

1. **Parse once, execute many**: The parser binds callbacks at parse time, so create the parser once and reuse it for multiple executions.

2. **Short-circuit evaluation**: Boolean expressions use short-circuit evaluation (`and`/`or`), so place cheaper conditions first.

3. **PathAccessor implementation**: Path resolution performance heavily depends on your `PathAccessor` implementation. Consider caching or pre-computing paths.

4. **Warmup**: Benchmarks include a 1,000-iteration warmup phase to stabilize measurements.

---

## Dependencies

```toml
[dependencies]
logos = "0.14"    # Lexer generator
chumsky = "0.12"  # Parser combinators
```

---

## License

See LICENSE file in the project root.
