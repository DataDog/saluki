//! Criterion benchmarks for OTTL parser
//!
//! Run with: `cargo bench -p ottl`

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use ottl::{CallbackMap, EnumMap, EvalContext, OttlParser, Parser, PathAccessor, PathResolver, Value};

// =====================================================================================================================
// Benchmark Helpers
// =====================================================================================================================

/// Mock path accessor for benchmarks
#[derive(Debug)]
struct BenchPathAccessor {
    path: String,
}

impl PathAccessor for BenchPathAccessor {
    fn get(&self, _ctx: &EvalContext, _path: &str) -> ottl::Result<&Value> {
        static INT_VAL: Value = Value::Int(42);
        static BOOL_VAL: Value = Value::Bool(true);
        static FLOAT_VAL: Value = Value::Float(6.14);

        if self.path.contains("int") || self.path.contains("count") || self.path.contains("status") {
            Ok(&INT_VAL)
        } else if self.path.contains("bool") || self.path.contains("enabled") {
            Ok(&BOOL_VAL)
        } else {
            Ok(&FLOAT_VAL)
        }
    }

    fn set(&self, _ctx: &mut EvalContext, _path: &str, _value: &Value) -> ottl::Result<()> {
        Ok(())
    }
}

fn bench_path_resolver() -> PathResolver {
    Arc::new(|path: &str| -> ottl::Result<Arc<dyn PathAccessor + Send + Sync>> {
        Ok(Arc::new(BenchPathAccessor { path: path.to_string() }))
    })
}

fn stub_context() -> EvalContext {
    Box::new(())
}

// =====================================================================================================================

// Execute Benchmarks
// =====================================================================================================================

fn bench_execute_math_simple(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let resolver = bench_path_resolver();

    let expression = "1 + 2 * 3 - 4 / 2";
    let parser = Parser::new(&editors, &converters, &enums, &resolver, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = stub_context();

    c.bench_function("execute_math_simple", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

fn bench_execute_math_complex(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let resolver = bench_path_resolver();

    let expression = "((1 + 2) * (3 + 4)) / ((5 - 2) * (6 - 4)) + (10 * (2 + 3))";
    let parser = Parser::new(&editors, &converters, &enums, &resolver, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = stub_context();

    c.bench_function("execute_math_complex", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

fn bench_execute_bool_comparisons(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let resolver = bench_path_resolver();

    let expression = "(1 < 2) and (3 > 1) or (5 == 5) and not (10 < 5)";
    let parser = Parser::new(&editors, &converters, &enums, &resolver, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = stub_context();

    c.bench_function("execute_bool_comparisons", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

fn bench_execute_with_paths(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let resolver = bench_path_resolver();

    let expression = "my.int.value + other.int.count * 2 - third.float.value";
    let parser = Parser::new(&editors, &converters, &enums, &resolver, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = stub_context();

    c.bench_function("execute_with_paths", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

fn bench_execute_bool_with_paths(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let resolver = bench_path_resolver();

    let expression = "my.bool.enabled and (my.int.status > 0) or (other.int.count < 100)";
    let parser = Parser::new(&editors, &converters, &enums, &resolver, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = stub_context();

    c.bench_function("execute_bool_with_paths", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

fn bench_execute_with_converters(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // Simple converter that adds two numbers
    converters.insert(
        "Add".to_string(),
        Arc::new(|_ctx: &mut EvalContext, args: Vec<ottl::Argument>| {
            let a = match args.first().map(|a| a.value()) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            let b = match args.get(1).map(|a| a.value()) {
                Some(Value::Int(n)) => *n,
                _ => 0,
            };
            Ok(Value::Int(a + b))
        }),
    );

    // Converter that returns length
    converters.insert(
        "Len".to_string(),
        Arc::new(
            |_ctx: &mut EvalContext, args: Vec<ottl::Argument>| match args.first().map(|a| a.value()) {
                Some(Value::String(s)) => Ok(Value::Int(s.len() as i64)),
                Some(Value::List(l)) => Ok(Value::Int(l.len() as i64)),
                _ => Ok(Value::Int(0)),
            },
        ),
    );

    let enums = EnumMap::new();
    let resolver = bench_path_resolver();

    let expression = "Add(1, 2) + Add(3, 4) * Add(5, 6)";
    let parser = Parser::new(&editors, &converters, &enums, &resolver, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = stub_context();

    c.bench_function("execute_with_converters", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

fn bench_execute_complex_realistic(c: &mut Criterion) {
    let mut editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // set editor (does nothing in benchmark)
    editors.insert(
        "set".to_string(),
        Arc::new(|_ctx: &mut EvalContext, _args: Vec<ottl::Argument>| Ok(Value::Nil)),
    );

    // Concat converter
    converters.insert(
        "Concat".to_string(),
        Arc::new(|_ctx: &mut EvalContext, args: Vec<ottl::Argument>| {
            let mut result = String::new();
            for arg in &args {
                if let Value::String(s) = arg.value() {
                    result.push_str(s);
                }
            }
            Ok(Value::String(result))
        }),
    );

    // IsMatch converter
    converters.insert(
        "IsMatch".to_string(),
        Arc::new(|_ctx: &mut EvalContext, args: Vec<ottl::Argument>| {
            let s = match args.first().map(|a| a.value()) {
                Some(Value::String(s)) => s.clone(),
                _ => String::new(),
            };
            let pattern = match args.get(1).map(|a| a.value()) {
                Some(Value::String(p)) => p.clone(),
                _ => String::new(),
            };
            Ok(Value::Bool(s.contains(&pattern)))
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);
    enums.insert("STATUS_ERROR".to_string(), 500);

    let resolver = bench_path_resolver();

    let expression = r#"set(my.int.value, 100) where (my.int.status == STATUS_OK or my.int.status < STATUS_ERROR) and my.bool.enabled"#;
    let parser = Parser::new(&editors, &converters, &enums, &resolver, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = stub_context();

    c.bench_function("execute_complex_realistic", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

// =====================================================================================================================

// Parser Creation Benchmark
// =====================================================================================================================

fn bench_parser_creation(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let resolver = bench_path_resolver();

    let expression = "((1 + 2) * (3 + 4)) / ((5 - 2) * (6 - 4)) + (10 * (2 + 3))";

    c.bench_function("parser_creation", |b| {
        b.iter(|| {
            let parser = Parser::new(
                black_box(&editors),
                black_box(&converters),
                black_box(&enums),
                black_box(&resolver),
                black_box(expression),
            );
            black_box(parser)
        })
    });
}

// =====================================================================================================================
// Criterion Groups
// =====================================================================================================================

criterion_group!(
    benches,
    bench_execute_math_simple,
    bench_execute_math_complex,
    bench_execute_bool_comparisons,
    bench_execute_with_paths,
    bench_execute_bool_with_paths,
    bench_execute_with_converters,
    bench_execute_complex_realistic,
    bench_parser_creation,
);

criterion_main!(benches);
