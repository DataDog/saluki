//! Editor/converter statement tests: conditional execution, evaluated arguments, indexed writes, converter
//! indexing, and named arguments.

use std::sync::{Arc, Mutex};

use super::{empty_path_resolver_map, UnitFamily};
use crate::parser::ast::Field;
use crate::parser::Parser;
use crate::{
    CallbackMap, EnumMap, EvalContextFamily, IndexExpr, OttlParser, PathAccessor, PathResolver, PathResolverMap, Value,
};

/// Structure to capture editor call information
#[derive(Debug, Clone, Default)]
struct EditorCallCapture {
    called: bool,
    first_arg: Option<Value>,
    second_arg: Option<Value>,
}

/// PathAccessor that supports both get and set, with tracking
#[derive(Debug)]
struct TrackingPathAccessor {
    int_value: Value,
    status_code: Value,
    target_path: Value, // Dummy value for "target" path
    set_calls: Mutex<Vec<(String, Value)>>,
}

impl PathAccessor<UnitFamily> for TrackingPathAccessor {
    fn get<'a>(&self, _ctx: &(), fields: &[Field]) -> crate::Result<Value> {
        let path: String = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(".");
        let v = match path.as_str() {
            "my.int.value" => self.int_value.clone(),
            "status_code" => self.status_code.clone(),
            "target" | "x" => self.target_path.clone(),
            _ => return Err(format!("Unknown path: {}", path).into()),
        };
        if let Some(last) = fields.last() {
            crate::helpers::apply_indexes(v, &last.keys)
        } else {
            Ok(v)
        }
    }

    fn set<'a>(&self, _ctx: &mut (), fields: &[Field], value: &Value) -> crate::Result<()> {
        let has_keys = fields.iter().any(|f| !f.keys.is_empty());
        if has_keys {
            return Err("TrackingPathAccessor: indexed set not supported".into());
        }
        let path: String = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(".");
        self.set_calls.lock().unwrap().push((path, value.clone()));
        Ok(())
    }
}

/// Create a PathResolverMap with tracking accessor for target, my.int.value, `status_code`
fn tracking_path_resolver_map(
    int_value: i64, status_code: i64,
) -> (PathResolverMap<UnitFamily>, Arc<TrackingPathAccessor>) {
    let accessor = Arc::new(TrackingPathAccessor {
        int_value: Value::Int(int_value),
        status_code: Value::Int(status_code),
        target_path: Value::Nil, // Dummy value for target path
        set_calls: Mutex::new(Vec::new()),
    });
    let accessor_clone = accessor.clone();
    let resolver: PathResolver<UnitFamily> =
        Arc::new(move || -> crate::Result<Arc<dyn PathAccessor<UnitFamily>>> { Ok(accessor_clone.clone()) });
    let mut m = PathResolverMap::new();
    m.insert("target".to_string(), resolver.clone());
    m.insert("x".to_string(), resolver.clone());
    m.insert("my.int.value".to_string(), resolver.clone());
    m.insert("status_code".to_string(), resolver);
    (m, accessor)
}

#[test]
fn editor_runs_when_condition_is_true() {
    // Setup: condition will be TRUE
    // my.int.value = 50 (> 0)
    // status_code = 200 (== STATUS_OK)

    let call_capture = Arc::new(Mutex::new(EditorCallCapture::default()));
    let capture_clone = call_capture.clone();

    let mut editors = CallbackMap::new();
    editors.insert(
        "set".to_string(),
        Arc::new(move |args: &mut dyn crate::Args| {
            let mut capture = capture_clone.lock().unwrap();
            capture.called = true;
            capture.first_arg = args.get(0).ok();
            capture.second_arg = args.get(1).ok();
            Ok(Value::Nil)
        }),
    );

    let mut converters = CallbackMap::new();
    // Sum converter: Sum(a, b) -> a + b
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: first argument must be numeric".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: second argument must be numeric".into()),
            };
            Ok(Value::Float(a + b))
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_WEIGHT".to_string(), 100);
    enums.insert("STATUS_OK".to_string(), 200);

    // my.int.value = 50, status_code = 200 (matches STATUS_OK)
    let (path_resolvers, _accessor) = tracking_path_resolver_map(50, 200);

    // Expression:
    // set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK
    // Sum(100, 50) * 1.5 = 150 * 1.5 = 225.0
    // Condition: 50 > 0 (true) and 200 == 200 (true) -> true
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Nil); // Editor returns Nil

    // Verify editor was called
    let capture = call_capture.lock().unwrap();
    assert!(capture.called, "Editor 'set' should have been called");

    // Verify first argument (path "target" resolves to Nil in our mock)
    assert_eq!(
        capture.first_arg,
        Some(Value::Nil),
        "First argument should be the resolved path value (Nil)"
    );

    // Verify second argument (computed value = 225.0)
    // Sum(STATUS_WEIGHT=100, my.int.value=50) * 1.5 = 150 * 1.5 = 225.0
    assert_eq!(
        capture.second_arg,
        Some(Value::Float(225.0)),
        "Second argument should be Sum(100, 50) * 1.5 = 225.0"
    );
}

#[test]
fn editor_is_skipped_when_condition_is_false() {
    // Setup: condition will be FALSE
    // my.int.value = -10 (NOT > 0)
    // status_code = 200 (== STATUS_OK, but first part is false)

    let call_capture = Arc::new(Mutex::new(EditorCallCapture::default()));
    let capture_clone = call_capture.clone();

    let mut editors = CallbackMap::new();
    editors.insert(
        "set".to_string(),
        Arc::new(move |args: &mut dyn crate::Args| {
            let mut capture = capture_clone.lock().unwrap();
            capture.called = true;
            capture.first_arg = args.get(0).ok();
            capture.second_arg = args.get(1).ok();
            Ok(Value::Nil)
        }),
    );

    let mut converters = CallbackMap::new();
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: first argument must be numeric".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v as f64,
                Value::Float(v) => v,
                _ => return Err("Sum: second argument must be numeric".into()),
            };
            Ok(Value::Float(a + b))
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_WEIGHT".to_string(), 100);
    enums.insert("STATUS_OK".to_string(), 200);

    // my.int.value = -10 (negative!), status_code = 200
    let (path_resolvers, _accessor) = tracking_path_resolver_map(-10, 200);

    // Expression:
    // set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK
    // Condition: -10 > 0 (FALSE) and 200 == 200 (true) -> false (short-circuit)
    // Editor should NOT be called
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "set(target, Sum(STATUS_WEIGHT, my.int.value) * 1.5) where my.int.value > 0 and status_code == STATUS_OK",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Nil); // Still returns Nil

    // Verify editor was NOT called
    let capture = call_capture.lock().unwrap();
    assert!(
        !capture.called,
        "Editor 'set' should NOT have been called when condition is false"
    );
    assert!(capture.first_arg.is_none(), "No arguments should be captured");
    assert!(capture.second_arg.is_none(), "No arguments should be captured");
}

#[test]
fn editor_receives_evaluated_list_of_maps() {
    // Test: set(x, [{"id": 1, "value": Double(5.0)}, {"id": 2, "value": STATUS_OK}, {"id": 3, "value": my.int.value}])
    // Expected result: x = [{"id": 1, "value": 10.0}, {"id": 2, "value": 200}, {"id": 3, "value": 73}]

    let call_capture = Arc::new(Mutex::new(EditorCallCapture::default()));
    let capture_clone = call_capture.clone();

    let mut editors = CallbackMap::new();
    editors.insert(
        "set".to_string(),
        Arc::new(move |args: &mut dyn crate::Args| {
            let mut capture = capture_clone.lock().unwrap();
            capture.called = true;
            capture.first_arg = args.get(0).ok();
            capture.second_arg = args.get(1).ok();
            Ok(Value::Nil)
        }),
    );

    let mut converters = CallbackMap::new();
    // Double converter: Double(x) -> x * 2
    converters.insert(
        "Double".to_string(),
        Arc::new(|args: &mut dyn crate::Args| match args.get(0)? {
            Value::Int(v) => Ok(Value::Int(v * 2)),
            Value::Float(v) => Ok(Value::Float(v * 2.0)),
            _ => Err("Double: argument must be numeric".into()),
        }),
    );

    let mut enums = EnumMap::new();
    enums.insert("STATUS_OK".to_string(), 200);

    // my.int.value = 73
    let (path_resolvers, _accessor) = tracking_path_resolver_map(73, 200);

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "set(x, [{\"id\": 1, \"value\": Double(5.0) ,}, {\"id\": 2, \"value\": STATUS_OK}, {\"id\": 3, \"value\": my.int.value}])",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Nil);

    // Verify editor was called
    let capture = call_capture.lock().unwrap();
    assert!(capture.called, "Editor 'set' should have been called");

    // Verify first argument (path "x" resolves to Nil in our mock)
    assert_eq!(
        capture.first_arg,
        Some(Value::Nil),
        "First argument should be the resolved path value"
    );

    // Verify second argument - list of maps with computed values
    // Expected: [{"id": 1, "value": 10.0}, {"id": 2, "value": 200}, {"id": 3, "value": 73}]
    use std::collections::HashMap;

    let mut map1 = HashMap::new();
    map1.insert("id".to_string(), Value::Int(1));
    map1.insert("value".to_string(), Value::Float(10.0));

    let mut map2 = HashMap::new();
    map2.insert("id".to_string(), Value::Int(2));
    map2.insert("value".to_string(), Value::Int(200));

    let mut map3 = HashMap::new();
    map3.insert("id".to_string(), Value::Int(3));
    map3.insert("value".to_string(), Value::Int(73));

    let expected_list = Value::List(vec![Value::Map(map1), Value::Map(map2), Value::Map(map3)]);

    assert_eq!(
        capture.second_arg,
        Some(expected_list),
        "Second argument should be the list of maps with computed values"
    );
}

// ============================================================================
// Indexed write-back through an editor
// ============================================================================

/// Context family for tests using MyValueListContext.
struct ListCtxFamily;

impl EvalContextFamily for ListCtxFamily {
    type Context<'a> = MyValueListContext;
}

/// Context that holds a single list at path "my.value"; supports get/set by index.
#[derive(Debug)]
struct MyValueListContext {
    list: Vec<Value>,
}

/// PathAccessor for "my.value" that reads/writes a list stored in context; supports indexed get/set.
#[derive(Debug)]
struct MyValueListAccessor;

impl PathAccessor<ListCtxFamily> for MyValueListAccessor {
    fn get<'a>(&self, ctx: &MyValueListContext, fields: &[Field]) -> crate::Result<Value> {
        let path: String = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(".");
        if path != "my.value" {
            return Err(format!("Unknown path: {}", path).into());
        }
        let value = Value::List(ctx.list.clone());
        if let Some(last) = fields.last() {
            crate::helpers::apply_indexes(value, &last.keys)
        } else {
            Ok(value)
        }
    }

    fn set<'a>(&self, ctx: &mut MyValueListContext, fields: &[Field], value: &Value) -> crate::Result<()> {
        let path: String = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(".");
        if path != "my.value" {
            return Err(format!("Unknown path: {}", path).into());
        }

        let last_keys = fields.last().map(|f| &f.keys[..]).unwrap_or(&[]);
        if last_keys.len() != 1 {
            return Err("my.value: exactly 1 index required".into());
        }
        let idx0 = match &last_keys[0] {
            IndexExpr::Int(i) => *i,
            IndexExpr::String(_) => return Err("my.value: integer index required".into()),
        };

        if idx0 >= ctx.list.len() {
            return Err(format!("Index {} out of bounds", idx0).into());
        }
        ctx.list[idx0] = value.clone();
        Ok(())
    }
}

#[test]
fn editor_writes_back_through_indexed_path() {
    // Context holds my.value as list [1, 2] (so my.value[0]=1, my.value[1]=2).
    // Execute add(my.value[1], 10): editor reads my.value[1] (2), adds 10, writes back via set.
    // Then assert my.value[1] == 12.
    let mut editors = CallbackMap::new();
    editors.insert(
        "add".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v,
                _ => return Err("add: first argument must be int".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v,
                _ => return Err("add: second argument must be int".into()),
            };
            args.set(0, &Value::Int(a + b))?;
            Ok(Value::Nil)
        }),
    );

    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    let resolver: PathResolver<ListCtxFamily> =
        Arc::new(|| Ok(Arc::new(MyValueListAccessor) as Arc<dyn PathAccessor<ListCtxFamily>>));
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("my.value".to_string(), resolver);

    let mut ctx = MyValueListContext {
        list: vec![Value::Int(1), Value::Int(2)],
    };
    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "add(my.value[1], 10)");
    assert!(parser.is_error().is_ok(), "parse should succeed");
    let result = parser.execute(&mut ctx);
    assert!(result.is_ok(), "execute should succeed: {:?}", result);

    assert_eq!(ctx.list.len(), 2);
    assert_eq!(ctx.list[0], Value::Int(1), "my.value[0] unchanged");
    assert_eq!(
        ctx.list[1],
        Value::Int(12),
        "my.value[1] should be 12 after add(my.value[1], 10)"
    );
}

// ============================================================================
// Converter indexing and named arguments
// ============================================================================

#[test]
fn converter_result_can_be_indexed() {
    // Test: Split("a,b,c", ",")[0] should return "a"
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let enums = EnumMap::new();

    // Split converter: splits string by delimiter, returns list
    converters.insert(
        "Split".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let text = match args.get(0)? {
                Value::String(s) => s,
                _ => return Err("Split first argument must be string".into()),
            };
            let delimiter = match args.get(1)? {
                Value::String(s) => s,
                _ => return Err("Split second argument must be string".into()),
            };
            let parts: Vec<Value> = text.split(delimiter.as_ref()).map(Value::string).collect();
            Ok(Value::List(parts))
        }),
    );

    let path_resolvers = empty_path_resolver_map();

    // Split("a,b,c", ",")[0] == "a"
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Split(\"a,b,c\", \",\")[0] == \"a\"",
    );
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "Split(\"a,b,c\", \",\")[0] should equal \"a\""
    );
}

#[test]
fn converter_accepts_named_arguments() {
    // Test: Convert(value=10, format="hex") with named arguments
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let enums = EnumMap::new();

    // Convert converter: converts value based on format
    // Uses named arguments: value and format
    converters.insert(
        "Convert".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            // Find arguments by name or positional fallback
            let value_val = args.get_named("value").unwrap_or_else(|| args.get(0))?;
            let format_val = args.get_named("format").unwrap_or_else(|| args.get(1))?;

            let value = match value_val {
                Value::Int(n) => n,
                _ => return Err("value must be integer".into()),
            };
            let format = match format_val {
                Value::String(s) => s,
                _ => return Err("format must be string".into()),
            };

            match format.as_ref() {
                "hex" => Ok(Value::string(format!("{:x}", value))),
                "binary" => Ok(Value::string(format!("{:b}", value))),
                "octal" => Ok(Value::string(format!("{:o}", value))),
                _ => Ok(Value::string(value.to_string())),
            }
        }),
    );

    let path_resolvers = empty_path_resolver_map();

    // Convert(value=10, format="hex") == "a"
    let parser: Parser<UnitFamily> = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Convert(value=10, format=\"hex\") == \"a\"",
    );
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "Convert(value=10, format=\"hex\") should equal \"a\""
    );
}
