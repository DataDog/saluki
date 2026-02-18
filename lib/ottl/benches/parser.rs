//! Criterion benchmarks for OTTL parser
//!
//! Run with: `cargo bench -p ottl`

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use ottl::{
    helpers, Args, CallbackMap, EnumMap, IndexExpr, OttlParser, Parser, PathAccessor, PathResolver,
    PathResolverMap, Value,
};

// =====================================================================================================================
// Benchmark context and path accessors (from tests.rs bench_execute_complex_realistic)
// =====================================================================================================================

#[derive(Debug)]
struct BenchContext {
    my_int_value: i64,
    my_int_status: i64,
    my_bool_enabled: bool,
}

impl BenchContext {
    fn new() -> Self {
        Self {
            my_int_value: 42,
            my_int_status: 200,
            my_bool_enabled: true,
        }
    }
}

#[derive(Debug)]
struct BenchPathAccessorIntValue;

impl PathAccessor<BenchContext> for BenchPathAccessorIntValue {
    fn get(&self, ctx: &BenchContext, path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let v = if path == "my.int.value" {
            Value::Int(ctx.my_int_value)
        } else {
            Value::Nil
        };
        helpers::apply_indexes(v, indexes)
    }
    fn set(&self, ctx: &mut BenchContext, path: &str, indexes: &[IndexExpr], value: &Value) -> ottl::Result<()> {
        if !indexes.is_empty() {
            return Err("BenchPathAccessorIntValue: indexed set not supported".into());
        }
        if path == "my.int.value" {
            if let Value::Int(v) = value {
                ctx.my_int_value = *v;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BenchPathAccessorIntStatus;

impl PathAccessor<BenchContext> for BenchPathAccessorIntStatus {
    fn get(&self, ctx: &BenchContext, path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let v = if path == "my.int.status" {
            Value::Int(ctx.my_int_status)
        } else {
            Value::Nil
        };
        helpers::apply_indexes(v, indexes)
    }
    fn set(&self, ctx: &mut BenchContext, path: &str, indexes: &[IndexExpr], value: &Value) -> ottl::Result<()> {
        if !indexes.is_empty() {
            return Err("BenchPathAccessorIntStatus: indexed set not supported".into());
        }
        if path == "my.int.status" {
            if let Value::Int(v) = value {
                ctx.my_int_status = *v;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BenchPathAccessorBoolEnabled;

impl PathAccessor<BenchContext> for BenchPathAccessorBoolEnabled {
    fn get(&self, ctx: &BenchContext, path: &str, indexes: &[IndexExpr]) -> ottl::Result<Value> {
        let v = if path == "my.bool.enabled" {
            Value::Bool(ctx.my_bool_enabled)
        } else {
            Value::Nil
        };
        helpers::apply_indexes(v, indexes)
    }
    fn set(&self, ctx: &mut BenchContext, path: &str, indexes: &[IndexExpr], value: &Value) -> ottl::Result<()> {
        if !indexes.is_empty() {
            return Err("BenchPathAccessorBoolEnabled: indexed set not supported".into());
        }
        if path == "my.bool.enabled" {
            if let Value::Bool(v) = value {
                ctx.my_bool_enabled = *v;
            }
        }
        Ok(())
    }
}

// =====================================================================================================================
// Criterion: parser creation
// =====================================================================================================================

fn bench_parser_creation(c: &mut Criterion) {
    let mut editors = CallbackMap::new();
    let converters = CallbackMap::new();

    editors.insert(
        "set".to_string(),
        Arc::new(|args: &mut dyn Args<BenchContext>| {
            let value = args.get(1)?;
            args.set(0, &value)?;
            Ok(Value::Nil)
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);
    enums.insert("STATUS_ERROR".to_string(), 500);

    let resolver_int_value: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntValue) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let resolver_int_status: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntStatus) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let resolver_bool_enabled: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorBoolEnabled) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("my.int.value".to_string(), resolver_int_value);
    path_resolvers.insert("my.int.status".to_string(), resolver_int_status);
    path_resolvers.insert("my.bool.enabled".to_string(), resolver_bool_enabled);

    let expression = r#"set(my.int.value, my.int.status + 100) where (my.int.status == STATUS_OK or my.int.status < STATUS_ERROR) and my.bool.enabled"#;

    c.bench_function("parser_creation", |b| {
        b.iter(|| {
            let parser = Parser::<BenchContext>::new(&editors, &converters, &enums, &path_resolvers, black_box(expression));
            black_box(parser)
        })
    });
}

// =====================================================================================================================
// Criterion: complex real-world-like expression (from bench_execute_complex_realistic)
// =====================================================================================================================

fn bench_execute_complex_realistic(c: &mut Criterion) {
    let mut editors = CallbackMap::new();
    let converters = CallbackMap::new();

    editors.insert(
        "set".to_string(),
        Arc::new(|args: &mut dyn Args<BenchContext>| {
            let value = args.get(1)?;
            args.set(0, &value)?;
            Ok(Value::Nil)
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);
    enums.insert("STATUS_ERROR".to_string(), 500);

    let resolver_int_value: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntValue) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let resolver_int_status: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntStatus) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let resolver_bool_enabled: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorBoolEnabled) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("my.int.value".to_string(), resolver_int_value);
    path_resolvers.insert("my.int.status".to_string(), resolver_int_status);
    path_resolvers.insert("my.bool.enabled".to_string(), resolver_bool_enabled);

    let expression = r#"set(my.int.value, my.int.status + 100) where (my.int.status == STATUS_OK or my.int.status < STATUS_ERROR) and my.bool.enabled"#;
    let parser = Parser::<BenchContext>::new(&editors, &converters, &enums, &path_resolvers, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = BenchContext::new();

    c.bench_function("execute_complex_realistic", |b| {
        b.iter(|| {
            ctx.my_int_value = 42;
            ctx.my_int_status = 200;
            ctx.my_bool_enabled = true;
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

// =====================================================================================================================
// Criterion: execute — simple math expression only
// =====================================================================================================================

fn bench_execute_math_simple(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = PathResolverMap::<()>::new();

    // 1+2*(9-2)/2 = 1 + 2*7/2 = 1 + 7 = 8 (math-only RootExpr::MathExpression)
    let expression = "1+2*(9-2)/2";
    let parser = Parser::<()>::new(&editors, &converters, &enums, &path_resolvers, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = ();

    c.bench_function("execute_math_simple", |b| {
        b.iter(|| {
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

// =====================================================================================================================
// Criterion: execute — boolean expression (enums, paths, negation, and/or, converters)
// =====================================================================================================================

fn bench_execute_bool_enums_paths_converters(c: &mut Criterion) {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();

    // Converter used in condition: returns bool
    converters.insert(
        "IsDisabled".to_string(),
        Arc::new(|_args: &mut dyn Args<BenchContext>| Ok(Value::Bool(false))),
    );
    converters.insert(
        "BoolConv".to_string(),
        Arc::new(|_args: &mut dyn Args<BenchContext>| Ok(Value::Bool(true))),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);

    let resolver_int_status: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntStatus) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let resolver_bool_enabled: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorBoolEnabled) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("my.int.status".to_string(), resolver_int_status);
    path_resolvers.insert("my.bool.enabled".to_string(), resolver_bool_enabled);

    // (path == enum or path) and not converter() and converter()
    let expression = r#"(my.int.status == STATUS_OK or my.bool.enabled) and not IsDisabled() and BoolConv()"#;
    let parser = Parser::<BenchContext>::new(&editors, &converters, &enums, &path_resolvers, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = BenchContext::new();

    c.bench_function("execute_bool_enums_paths_converters", |b| {
        b.iter(|| {
            ctx.my_int_status = 200;
            ctx.my_bool_enabled = true;
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

// =====================================================================================================================
// Criterion: execute — editor call only
// =====================================================================================================================

fn bench_execute_editor_call(c: &mut Criterion) {
    let mut editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    editors.insert(
        "set".to_string(),
        Arc::new(|args: &mut dyn Args<BenchContext>| {
            let value = args.get(1)?;
            args.set(0, &value)?;
            Ok(Value::Nil)
        }),
    );

    let resolver_int_value: PathResolver<BenchContext> =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntValue) as Arc<dyn PathAccessor<BenchContext> + Send + Sync>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("my.int.value".to_string(), resolver_int_value);

    let expression = "set(my.int.value, 42)";
    let parser = Parser::<BenchContext>::new(&editors, &converters, &enums, &path_resolvers, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx = BenchContext::new();

    c.bench_function("execute_editor_call", |b| {
        b.iter(|| {
            ctx.my_int_value = 0;
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

criterion_group!(
    benches,
    bench_parser_creation,
    bench_execute_complex_realistic,
    bench_execute_math_simple,
    bench_execute_bool_enums_paths_converters,
    bench_execute_editor_call,
);
criterion_main!(benches);
