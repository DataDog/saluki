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
- Extensible path system via `PathResolverMap` (path → `PathResolver`)

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
    Nil,                           // null/nil
    Bool(bool),                    // true, false
    Int(i64),                      // 42, -10
    Float(f64),                    // 3.14, .5
    String(Arc<str>),              // "hello" (Arc for cheap clone)
    Bytes(Arc<[u8]>),             // 0xDEADBEEF (Arc for cheap clone)
    List(Vec<Value>),             // [1, 2, 3]
    Map(HashMap<String, Value>)   // {"key": "value"}
}
```

Helper methods: `Value::string(impl Into<Arc<str>>)` and `Value::bytes(impl Into<Arc<[u8]>>)` for cheap construction.

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
// Lazy argument evaluation — arguments are evaluated only when requested (zero allocation)
pub trait Args {
    fn ctx(&mut self) -> &mut EvalContext;
    fn len(&self) -> usize;
    fn get(&mut self, index: usize) -> Result<Value>;
    fn name(&self, index: usize) -> Option<&str>;
    fn get_named(&mut self, name: &str) -> Option<Result<Value>>;
    fn set(&mut self, index: usize, value: &Value) -> Result<()>;
}

// Callback function for editors and converters (receives Args, not Vec<Argument>)
pub type CallbackFn = Arc<dyn Fn(&mut dyn Args) -> Result<Value> + Send + Sync>;

// Map of function names → their implementations
pub type CallbackMap = HashMap<String, CallbackFn>;

// Map of enum values → their numeric equivalents
pub type EnumMap = HashMap<String, i64>;

// Path resolver: returns a PathAccessor for one path (resolved at parse time)
pub type PathResolver = Arc<dyn Fn() -> Result<Arc<dyn PathAccessor + Send + Sync>> + Send + Sync>;

// Map from path string to PathResolver. Every path used in the expression must have an entry.
pub type PathResolverMap = HashMap<String, PathResolver>;
```

### PathAccessor

Trait for accessing values by path:

```rust
pub trait PathAccessor: fmt::Debug {
    fn get(&self, ctx: &EvalContext, path: &str) -> Result<Value>;
    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> Result<()>;
}
```

---

## API

### Creating a Parser

```rust
use ottl::{Parser, OttlParser, CallbackMap, EnumMap, PathResolver, PathResolverMap};

let editors: CallbackMap = HashMap::new();
let converters: CallbackMap = HashMap::new();
let enums: EnumMap = HashMap::new();
let mut path_resolvers: PathResolverMap = HashMap::new();
path_resolvers.insert("resource.attributes".to_string(), Arc::new(|| { /* return Ok(Arc::new(...)) */ }));
path_resolvers.insert("status".to_string(), Arc::new(|| { /* ... */ }));

let parser = Parser::new(
    &editors,
    &converters,
    &enums,
    &path_resolvers,
    "set(resource.attributes[\"key\"], \"value\") where status == 200"
);
```

If any path in the expression is missing from `path_resolvers`, parsing fails with an error.

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
editors.insert("set".to_string(), Arc::new(|args: &mut dyn ottl::Args| {
    let value = args.get(1)?;
    args.set(0, &value)?;
    Ok(Value::Nil)
}));
```

### Converter

```rust
// OTTL: Concat(first_name, " ", last_name)

let mut converters = CallbackMap::new();
converters.insert("Concat".to_string(), Arc::new(|args: &mut dyn ottl::Args| {
    let mut result = String::new();
    for i in 0..args.len() {
        if let Ok(Value::String(s)) = args.get(i) {
            result.push_str(s.as_ref());
        }
    }
    Ok(Value::string(result))
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

### `parser/` (parser, ast, arena, eval)

Syntax analyzer based on [chumsky 0.12](https://crates.io/crates/chumsky). Parsing produces boxed AST types; they are then converted to an arena-based representation for cache-friendly execution.

**Boxed AST (parsing, in `ast.rs`):**

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

// Mathematical / logical expressions
enum MathExpr { Primary(ValueExpr), Negate(Box<MathExpr>), Binary { ... } }
enum BoolExpr { Literal(bool), Comparison { ... }, Converter(...), Path(PathExpr), Not(...), And(...), Or(...) }
```

Execution uses **arena-based** types (`ArenaRootExpr`, `ArenaBoolExpr`, `ArenaMathExpr`, `ArenaValueExpr`, `ArenaFunctionCall`, `ResolvedPath`) with indices instead of `Box` for better cache locality.

### `mod.rs`

Main library module, exports the public API:

- `Parser` — main parser
- `OttlParser` — public API trait
- `Value`, `Argument` — data types
- `Args` — trait for lazy argument evaluation in callbacks
- `CallbackFn`, `CallbackMap`, `EnumMap` — callback types
- `PathAccessor`, `PathResolver`, `PathResolverMap` — path handling types
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
# Run the benchmark
cargo bench -p ottl

# If you want to run benchmark from tests with manual control
cargo test bench_execute_complex_realistic -- --ignored --nocapture
```

### Benchmark Suite

Benchmarks live in `lib/ottl/benches/parser.rs`. Two kinds of benchmarks are included:

- **Parser creation**: measures `Parser::new(...)` (lexer + grammar parse + path resolution).
- **Execute**: measures only `parser.execute(ctx)`; the parser is built once outside the timed loop.

| Benchmark | What is measured | Expression / scenario |
|-----------|------------------|-------------------------|
| `parser_creation` | Time to create parser (lex + parse + path resolution) | `set(my.int.value, my.int.status + 100) where (my.int.status == STATUS_OK or ...) and my.bool.enabled` |
| `execute_complex_realistic` | Execute: editor + WHERE + paths + enums | Same as above |
| `execute_math_simple` | Execute: math expression only | `1+2*(9-2)/2` |
| `execute_bool_enums_paths_converters` | Execute: boolean with enums, paths, `not`, `and`/`or`, converters | `(my.int.status == STATUS_OK or my.bool.enabled) and not IsDisabled() and BoolConv()` |
| `execute_editor_call` | Execute: single editor call | `set(my.int.value, 42)` |

### Benchmark Results

Results from running `cargo bench -p ottl` (release build, 100 samples). Times are per iteration.

**N.B.:** Apple M4 Max, 64 GB was used to run benchmarks.

| Benchmark | Time (per iter) | Throughput (approx) |
|-----------|-----------------|----------------------|
| `parser_creation` | ~6.3 µs | ~158k parsers/sec |
| `execute_complex_realistic` | ~69 ns | ~14.5M exec/sec |
| `execute_math_simple` | ~41 ns | ~24.5M exec/sec |
| `execute_bool_enums_paths_converters` | ~46 ns | ~21.5M exec/sec |
| `execute_editor_call` | ~17.5 ns | ~57M exec/sec |


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
