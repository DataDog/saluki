//! Criterion benchmarks for OTTL parser
//!
//! Run with: `cargo bench -p ottl`

use std::hint::black_box;
use std::sync::Arc;

use criterion::{criterion_group, criterion_main, Criterion};
use ottl::{
    Args, CallbackMap, EnumMap, EvalContext, OttlParser, Parser, PathAccessor, PathResolver, PathResolverMap, Value,
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

impl PathAccessor for BenchPathAccessorIntValue {
    #[inline]
    fn get(&self, ctx: &EvalContext, path: &str) -> ottl::Result<Value> {
        if path == "my.int.value" {
            if let Some(bench_ctx) = ctx.downcast_ref::<BenchContext>() {
                return Ok(Value::Int(bench_ctx.my_int_value));
            }
        }
        Ok(Value::Nil)
    }

    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> ottl::Result<()> {
        if path == "my.int.value" {
            if let Some(bench_ctx) = ctx.downcast_mut::<BenchContext>() {
                if let Value::Int(v) = value {
                    bench_ctx.my_int_value = *v;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BenchPathAccessorIntStatus;

impl PathAccessor for BenchPathAccessorIntStatus {
    #[inline]
    fn get(&self, ctx: &EvalContext, path: &str) -> ottl::Result<Value> {
        if path == "my.int.status" {
            if let Some(bench_ctx) = ctx.downcast_ref::<BenchContext>() {
                return Ok(Value::Int(bench_ctx.my_int_status));
            }
        }
        Ok(Value::Nil)
    }

    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> ottl::Result<()> {
        if path == "my.int.status" {
            if let Some(bench_ctx) = ctx.downcast_mut::<BenchContext>() {
                if let Value::Int(v) = value {
                    bench_ctx.my_int_status = *v;
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
struct BenchPathAccessorBoolEnabled;

impl PathAccessor for BenchPathAccessorBoolEnabled {
    #[inline]
    fn get(&self, ctx: &EvalContext, path: &str) -> ottl::Result<Value> {
        if path == "my.bool.enabled" {
            if let Some(bench_ctx) = ctx.downcast_ref::<BenchContext>() {
                return Ok(Value::Bool(bench_ctx.my_bool_enabled));
            }
        }
        Ok(Value::Nil)
    }

    fn set(&self, ctx: &mut EvalContext, path: &str, value: &Value) -> ottl::Result<()> {
        if path == "my.bool.enabled" {
            if let Some(bench_ctx) = ctx.downcast_mut::<BenchContext>() {
                if let Value::Bool(v) = value {
                    bench_ctx.my_bool_enabled = *v;
                }
            }
        }
        Ok(())
    }
}

// =====================================================================================================================
// Criterion: complex real-world-like expression (from bench_execute_complex_realistic)
// =====================================================================================================================

fn bench_execute_complex_realistic(c: &mut Criterion) {
    let mut editors = CallbackMap::new();
    let converters = CallbackMap::new();

    editors.insert(
        "set".to_string(),
        Arc::new(|args: &mut dyn Args| {
            let value = args.get(1)?;
            args.set(0, &value)?;
            Ok(Value::Nil)
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);
    enums.insert("STATUS_ERROR".to_string(), 500);

    let resolver_int_value: PathResolver =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntValue) as Arc<dyn PathAccessor + Send + Sync>));
    let resolver_int_status: PathResolver =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorIntStatus) as Arc<dyn PathAccessor + Send + Sync>));
    let resolver_bool_enabled: PathResolver =
        Arc::new(|| Ok(Arc::new(BenchPathAccessorBoolEnabled) as Arc<dyn PathAccessor + Send + Sync>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("my.int.value".to_string(), resolver_int_value);
    path_resolvers.insert("my.int.status".to_string(), resolver_int_status);
    path_resolvers.insert("my.bool.enabled".to_string(), resolver_bool_enabled);

    let expression = r#"set(my.int.value, my.int.status + 100) where (my.int.status == STATUS_OK or my.int.status < STATUS_ERROR) and my.bool.enabled"#;
    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, expression);
    assert!(parser.is_error().is_ok(), "Parse failed");

    let mut ctx: EvalContext = Box::new(BenchContext::new());

    c.bench_function("execute_complex_realistic", |b| {
        b.iter(|| {
            if let Some(bench_ctx) = ctx.downcast_mut::<BenchContext>() {
                bench_ctx.my_int_value = 42;
                bench_ctx.my_int_status = 200;
                bench_ctx.my_bool_enabled = true;
            }
            let result = parser.execute(black_box(&mut ctx));
            black_box(result)
        })
    });
}

criterion_group!(benches, bench_execute_complex_realistic);
criterion_main!(benches);
