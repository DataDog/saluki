//! Parser and evaluation tests: value/argument types, math/boolean/enum/converter evaluation, and path/index
//! resolution.

use std::sync::Arc;

use super::{empty_path_resolver_map, parse_with_empty_maps, UnitFamily};
use crate::parser::ast::Field;
use crate::parser::Parser;
use crate::{
    Argument, CallbackMap, EnumMap, IndexExpr, OttlParser, PathAccessor, PathResolver, PathResolverMap, Value,
};

// ============================================================================
// Value / Argument types
// ============================================================================

#[test]
fn value_comparisons() {
    assert_eq!(Value::Bool(true), Value::Bool(true));
    assert_eq!(Value::Int(42), Value::Int(42));
    assert_eq!(Value::Float(6.18), Value::Float(6.18));
    assert_eq!(Value::String("hello".into()), Value::String("hello".into()));
    assert_eq!(Value::Nil, Value::Nil);

    assert_ne!(Value::Bool(true), Value::Bool(false));
    assert_ne!(Value::Int(1), Value::Int(2));
    assert_ne!(Value::Bool(true), Value::Int(1));
}

#[test]
fn argument_variants_carry_their_payload() {
    let pos = Argument::Positional(Value::Int(42));
    match &pos {
        Argument::Positional(v) => assert_eq!(v, &Value::Int(42)),
        _ => panic!("expected Positional"),
    }

    let named = Argument::Named {
        name: "foo".into(),
        value: Value::String("bar".into()),
    };
    match &named {
        Argument::Named { name, value } => {
            assert_eq!(name, "foo");
            assert_eq!(value, &Value::String("bar".into()));
        }
        _ => panic!("expected Named"),
    }
}

// ============================================================================
// Path accessors used by the parser integration tests
// ============================================================================

/// Stub `PathAccessor` that does nothing (for testing purposes).
#[derive(Debug)]
struct StubPathAccessor;

impl PathAccessor<UnitFamily> for StubPathAccessor {
    fn get<'a>(&self, _ctx: &(), _fields: &[Field]) -> crate::Result<Value> {
        Err("StubPathAccessor: get not implemented".into())
    }

    fn set<'a>(&self, _ctx: &mut (), _fields: &[Field], _value: &Value) -> crate::Result<()> {
        Err("StubPathAccessor: set not implemented".into())
    }
}

/// Create a path resolver map with stub accessor for each given path.
fn stub_path_resolver_for(paths: &[&str]) -> PathResolverMap<UnitFamily> {
    let stub: PathResolver<UnitFamily> =
        Arc::new(|| -> crate::Result<Arc<dyn PathAccessor<UnitFamily>>> { Ok(Arc::new(StubPathAccessor)) });
    paths.iter().map(|&p| (p.to_string(), stub.clone())).collect()
}

/// PathAccessor that returns predefined values for specific paths
#[derive(Debug)]
struct MockPathAccessor {
    bool_value: Value,
    int_value: Value,
}

impl PathAccessor<UnitFamily> for MockPathAccessor {
    fn get<'a>(&self, _ctx: &(), fields: &[Field]) -> crate::Result<Value> {
        let path: String = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(".");
        let v = match path.as_str() {
            "my.bool.value" => self.bool_value.clone(),
            "my.int.value" => self.int_value.clone(),
            _ => return Err(format!("Unknown path: {}", path).into()),
        };
        if let Some(last) = fields.last() {
            crate::helpers::apply_indexes(v, &last.keys)
        } else {
            Ok(v)
        }
    }

    fn set<'a>(&self, _ctx: &mut (), _fields: &[Field], _value: &Value) -> crate::Result<()> {
        Err("MockPathAccessor: set not implemented".into())
    }
}

/// Create a PathResolverMap with MockPathAccessor for my.bool.value and my.int.value
fn mock_path_resolver_map(bool_value: bool, int_value: i64) -> PathResolverMap<UnitFamily> {
    let accessor = Arc::new(MockPathAccessor {
        bool_value: Value::Bool(bool_value),
        int_value: Value::Int(int_value),
    });
    let resolver: PathResolver<UnitFamily> = Arc::new(move || Ok(accessor.clone()));
    let mut m = PathResolverMap::new();
    m.insert("my.bool.value".to_string(), resolver.clone());
    m.insert("my.int.value".to_string(), resolver);
    m
}

// ============================================================================
// Parser integration tests
// ============================================================================

#[test]
fn execution_surfaces_path_accessor_errors() {
    // stub_path_resolver_for provides resolvers so parsing succeeds;
    // execute fails because StubPathAccessor::get returns Err
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let path_resolvers = stub_path_resolver_for(&["stub.path"]);

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "stub.path == 1");

    assert!(parser.is_error().is_ok(), "Parsing should succeed");
    let result = parser.execute(&mut ());
    assert!(
        result.is_err(),
        "Execute should fail: stub accessor returns Err from get"
    );
}

#[test]
fn evaluates_integer_and_float_math() {
    // Integer math: -1 + 2*10 - 10/5 - (1+3*2) = -1 + 20 - 2 - 7 = 10
    let parser = parse_with_empty_maps("-1+   2*10 - 10/5 - (1+3*2)");
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(10));

    // Float with scientific notation: 1.0e2 + .5E1 - 2.e+1 = 100.0 + 5.0 - 20.0 = 85.0
    let parser = parse_with_empty_maps("1.0e2 + .5E1 - 2.e+1");
    if let Err(e) = parser.is_error() {
        panic!("Parser error (scientific notation): {}", e);
    }
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Float(85.0));

    // Mixed plain and scientific floats: 2.5 * 1.0e1 + .25E2 = 25.0 + 25.0 = 50.0
    let parser = parse_with_empty_maps("2.5 * 1.0e1 + .25E2");
    if let Err(e) = parser.is_error() {
        panic!("Parser error (mixed floats): {}", e);
    }
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Float(50.0));

    // Negative exponent: 5.0e-1 + 2.5e-1 = 0.5 + 0.25 = 0.75
    let parser = parse_with_empty_maps("5.0e-1 + 2.5e-1");
    if let Err(e) = parser.is_error() {
        panic!("Parser error (negative exponent): {}", e);
    }
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Float(0.75));
}

#[test]
fn evaluates_boolean_expression_with_math() {
    let parser = parse_with_empty_maps(
        "false or not (2 < (1 + 2)) or (0xDEADBEEF == nil) or (1 != 2) or (2 >= 1.5) and (true) and \"banana 🎉\" > \"apple\"",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    // false or (2 < 3) = false or true = true
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Bool(true));
}

#[test]
fn evaluates_boolean_expression_with_paths() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    // Create resolver map for my.bool.value and my.int.value
    let path_resolvers = mock_path_resolver_map(false, 2);

    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "my.bool.value or (my.int.value < (1 + 2))",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    // my.bool.value = false
    // my.int.value = 2
    // 1 + 2 = 3
    // my.int.value < 3 = 2 < 3 = true
    // false or true = true
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Bool(true));
}

#[test]
fn evaluates_math_with_converters() {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let enums = EnumMap::new();

    // Register Sum converter: Sum(a: int, b: int) -> int { a + b }
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v,
                _ => return Err("Sum: first argument must be int".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v,
                _ => return Err("Sum: second argument must be int".into()),
            };
            Ok(Value::Int(a + b))
        }),
    );

    let path_resolvers = mock_path_resolver_map(false, 0);

    // Expression: Sum(1, 2) + 10 * Sum(-1, 1)
    // Sum(1, 2) = 1 + 2 = 3
    // Sum(-1, 1) = -1 + 1 = 0
    // 10 * 0 = 0
    // 3 + 0 = 3
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Sum(1, 2) + 10 * Sum(-1, 1)",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(3));

    // Test: Sum(1,2) * Sum(2,4) = 3 * 6 = 18
    let parser2 = Parser::new(&editors, &converters, &enums, &path_resolvers, "Sum(1,2) * Sum(2,4)");

    if let Err(e) = parser2.is_error() {
        panic!("Parser error: {}", e);
    }

    let result2 = parser2.execute(&mut ());
    assert!(result2.is_ok(), "Execution should succeed: {:?}", result2);
    assert_eq!(result2.unwrap(), Value::Int(18));
}

#[test]
fn evaluates_math_with_enums() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let mut enums = EnumMap::new();

    // Register enum values
    enums.insert("MY_INT_VALUE1".to_string(), 1);
    enums.insert("MY_INT_VALUE200".to_string(), 200);
    enums.insert("MY_INT_VALUE199".to_string(), 199);

    let path_resolvers = empty_path_resolver_map();

    // Expression: MY_INT_VALUE200 - (MY_INT_VALUE1 + MY_INT_VALUE199)
    // 200 - (1 + 199) = 200 - 200 = 0
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "MY_INT_VALUE200 - (MY_INT_VALUE1 + MY_INT_VALUE199)",
    );

    // Check no parsing errors
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    // Execute and check result
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(0));

    // Test with unary minus before enum
    // Expression: -MY_INT_VALUE1 + MY_INT_VALUE200
    // -1 + 200 = 199
    let parser2 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "-MY_INT_VALUE1 + MY_INT_VALUE200",
    );

    if let Err(e) = parser2.is_error() {
        panic!("Parser error with unary minus: {}", e);
    }

    let result2 = parser2.execute(&mut ());
    assert!(
        result2.is_ok(),
        "Execution with unary minus should succeed: {:?}",
        result2
    );
    assert_eq!(result2.unwrap(), Value::Int(199));
}

#[test]
fn evaluates_boolean_expression_with_enums() {
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let mut enums = EnumMap::new();

    // Register enum values
    enums.insert("STATUS_OK".to_string(), 200);
    enums.insert("STATUS_NOT_FOUND".to_string(), 404);
    enums.insert("STATUS_ERROR".to_string(), 500);

    let path_resolvers = empty_path_resolver_map();

    // Test 1: STATUS_OK < STATUS_NOT_FOUND (200 < 404 = true)
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "STATUS_OK < STATUS_NOT_FOUND",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Bool(true));

    // Test 2: STATUS_ERROR == 500 (500 == 500 = true)
    let parser2 = Parser::new(&editors, &converters, &enums, &path_resolvers, "STATUS_ERROR == 500");

    if let Err(e) = parser2.is_error() {
        panic!("Parser error: {}", e);
    }

    let result2 = parser2.execute(&mut ());
    assert!(result2.is_ok(), "Execution should succeed: {:?}", result2);
    assert_eq!(result2.unwrap(), Value::Bool(true));

    // Test 3: Complex boolean with enums
    // (STATUS_OK < STATUS_NOT_FOUND) and (STATUS_ERROR > 400)
    // (200 < 404) and (500 > 400) = true and true = true
    let parser3 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "(((STATUS_OK < STATUS_NOT_FOUND))) and (STATUS_ERROR > 400)",
    );

    if let Err(e) = parser3.is_error() {
        panic!("Parser error: {}", e);
    }

    let result3 = parser3.execute(&mut ());
    assert!(result3.is_ok(), "Execution should succeed: {:?}", result3);
    assert_eq!(result3.unwrap(), Value::Bool(true));
}

#[test]
fn evaluates_enums_as_function_arguments() {
    let editors = CallbackMap::new();
    let mut converters = CallbackMap::new();
    let mut enums = EnumMap::new();

    // Register enum values
    enums.insert("VALUE_10".to_string(), 10);
    enums.insert("VALUE_20".to_string(), 20);
    enums.insert("VALUE_5".to_string(), 5);

    // Register Sum converter: Sum(a: int, b: int) -> int { a + b }
    converters.insert(
        "Sum".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v,
                _ => return Err("Sum: first argument must be int".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v,
                _ => return Err("Sum: second argument must be int".into()),
            };
            Ok(Value::Int(a + b))
        }),
    );

    // Register Multiply converter: Multiply(a: int, b: int) -> int { a * b }
    converters.insert(
        "Multiply".to_string(),
        Arc::new(|args: &mut dyn crate::Args| {
            let a = match args.get(0)? {
                Value::Int(v) => v,
                _ => return Err("Multiply: first argument must be int".into()),
            };
            let b = match args.get(1)? {
                Value::Int(v) => v,
                _ => return Err("Multiply: second argument must be int".into()),
            };
            Ok(Value::Int(a * b))
        }),
    );

    let path_resolvers = empty_path_resolver_map();

    // Test 1: Sum(VALUE_10, VALUE_20) + 0 = 10 + 20 + 0 = 30
    // Note: We add "+ 0" to force parsing as math expression (not bool)
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Sum(VALUE_10, VALUE_20) + 0",
    );

    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }

    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution should succeed: {:?}", result);
    assert_eq!(result.unwrap(), Value::Int(30));

    // Test 2: Multiply(VALUE_5, VALUE_10) * 1 = 5 * 10 * 1 = 50
    let parser2 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Multiply(VALUE_5, VALUE_10) * 1",
    );

    if let Err(e) = parser2.is_error() {
        panic!("Parser error: {}", e);
    }

    let result2 = parser2.execute(&mut ());
    assert!(result2.is_ok(), "Execution should succeed: {:?}", result2);
    assert_eq!(result2.unwrap(), Value::Int(50));

    // Test 3: Nested - Sum(Multiply(VALUE_5, VALUE_10), VALUE_20) + 0 = (5*10) + 20 + 0 = 70
    let parser3 = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "Sum(Multiply(VALUE_5, VALUE_10), VALUE_20) + 0",
    );

    if let Err(e) = parser3.is_error() {
        panic!("Parser error: {}", e);
    }

    let result3 = parser3.execute(&mut ());
    assert!(result3.is_ok(), "Execution should succeed: {:?}", result3);
    assert_eq!(result3.unwrap(), Value::Int(70));
}

// ============================================================================
// Path expression tests
// ============================================================================

/// PathAccessor that supports multi-level paths, index access, and key access
#[derive(Debug)]
struct PathExprAccessor {
    // resource.attributes.status = 200
    // resource.count = 10
    resource_status: Value,
    resource_count: Value,
    // items[0] = 5, items[1] = 3
    items: Value,
    // data["key"] = 100, data["multiplier"] = 2
    data: Value,
}

impl PathAccessor<UnitFamily> for PathExprAccessor {
    fn get<'a>(&self, _ctx: &(), fields: &[Field]) -> crate::Result<Value> {
        let path: String = fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>().join(".");
        let v = match path.as_str() {
            "resource.attributes.status" => self.resource_status.clone(),
            "resource.count" => self.resource_count.clone(),
            "items" => self.items.clone(),
            "data" => self.data.clone(),
            _ => return Err(format!("Unknown path: {}", path).into()),
        };
        if let Some(last) = fields.last() {
            crate::helpers::apply_indexes(v, &last.keys)
        } else {
            Ok(v)
        }
    }

    fn set<'a>(&self, _ctx: &mut (), _fields: &[Field], _value: &Value) -> crate::Result<()> {
        Err("PathExprAccessor: set not implemented".into())
    }
}

#[test]
fn evaluates_all_path_expression_forms() {
    // This test verifies all path expression types in one boolean expression:
    // - Multi-level paths: resource.attributes.status
    // - Index access by number: items[0], items[1]
    // - Index access by key: data["key"]
    // - Math expressions with paths: items[0] + items[1]

    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    use std::collections::HashMap;

    // Setup data map: {"key": 100, "multiplier": 2}
    let mut data_map = HashMap::new();
    data_map.insert("key".to_string(), Value::Int(100));
    data_map.insert("multiplier".to_string(), Value::Int(2));

    let accessor = Arc::new(PathExprAccessor {
        resource_status: Value::Int(200),
        resource_count: Value::Int(10),
        items: Value::List(vec![Value::Int(5), Value::Int(3)]),
        data: Value::Map(data_map),
    });
    let accessor_clone = accessor.clone();
    let resolver: PathResolver<UnitFamily> =
        Arc::new(move || -> crate::Result<Arc<dyn PathAccessor<UnitFamily>>> { Ok(accessor_clone.clone()) });
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("resource.attributes.status".to_string(), resolver.clone());
    path_resolvers.insert("items".to_string(), resolver.clone());
    path_resolvers.insert("data".to_string(), resolver);

    // Combined expression testing all path types:
    // (resource.attributes.status == 200) and (items[0] + items[1] == 8) and (data["key"] == 100)
    // - resource.attributes.status = 200 → true
    // - items[0] + items[1] = 5 + 3 = 8 → true
    // - data["key"] = 100 → true
    // Result: true and true and true = true
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        "(resource.attributes.status == 200) and (items[0] + items[1] == 8) and (data[\"key\"] == 100)",
    );
    if let Err(e) = parser.is_error() {
        panic!("Parser error: {}", e);
    }
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "Execution failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "Combined path expression should be true"
    );
}

// ============================================================================
// Per-field index tests (indexes on intermediate fields)
// ============================================================================

/// PathAccessor that interprets per-field keys on intermediate path segments.
///
/// Handles a nested structure shaped like:
///   resource.attributes["env"].length   -> 10
///   resource.attributes["env"].name     -> "production"
///   resource.tags["region"]["az"]       -> "us-east-1a"
///
/// This accessor inspects each field's keys individually, exercising the new
/// `Field { name, keys }` design where intermediate fields carry their own indexes.
#[derive(Debug)]
struct PerFieldIndexAccessor;

impl PathAccessor<UnitFamily> for PerFieldIndexAccessor {
    fn get<'a>(&self, _ctx: &(), fields: &[Field]) -> crate::Result<Value> {
        // Verify the accessor receives correctly structured fields
        match fields.len() {
            // resource.attributes["env"].length  -> 3 fields
            3 if fields[0].name == "resource" && fields[1].name == "attributes" && fields[2].name == "length" => {
                // attributes must have exactly one string key
                if let Some(IndexExpr::String(key)) = fields[1].keys.first() {
                    if key == "env" {
                        return Ok(Value::Int(10));
                    }
                }
                Err("attributes key not recognised".into())
            }
            // resource.attributes["env"].name  -> 3 fields
            3 if fields[0].name == "resource" && fields[1].name == "attributes" && fields[2].name == "name" => {
                if let Some(IndexExpr::String(key)) = fields[1].keys.first() {
                    if key == "env" {
                        return Ok(Value::string("production"));
                    }
                }
                Err("attributes key not recognised".into())
            }
            // resource.tags["region"]["az"]  -> 2 fields, second has 2 keys
            2 if fields[0].name == "resource" && fields[1].name == "tags" => {
                // tags must have exactly two string keys
                if fields[1].keys.len() == 2 {
                    if let (IndexExpr::String(k1), IndexExpr::String(k2)) = (&fields[1].keys[0], &fields[1].keys[1]) {
                        if k1 == "region" && k2 == "az" {
                            return Ok(Value::string("us-east-1a"));
                        }
                    }
                }
                Err("tags keys not recognised".into())
            }
            _ => {
                let desc: String = fields
                    .iter()
                    .map(|f| {
                        let keys: Vec<String> = f
                            .keys
                            .iter()
                            .map(|k| match k {
                                IndexExpr::String(s) => format!("[\"{}\" ]", s),
                                IndexExpr::Int(i) => format!("[{}]", i),
                            })
                            .collect();
                        format!("{}{}", f.name, keys.join(""))
                    })
                    .collect::<Vec<_>>()
                    .join(".");
                Err(format!("Unknown structured path: {}", desc).into())
            }
        }
    }

    fn set<'a>(&self, _ctx: &mut (), _fields: &[Field], _value: &Value) -> crate::Result<()> {
        Err("PerFieldIndexAccessor: set not implemented".into())
    }
}

#[test]
fn resolves_indexes_on_intermediate_path_fields() {
    // Tests the core new capability: indexes on INTERMEDIATE fields, not just the last one.
    // The path `resource.attributes["env"].length` has three fields:
    //   Field { name: "resource", keys: [] }
    //   Field { name: "attributes", keys: [String("env")] }   <- intermediate field with key
    //   Field { name: "length", keys: [] }
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    let accessor: Arc<dyn PathAccessor<UnitFamily>> = Arc::new(PerFieldIndexAccessor);
    let resolver: PathResolver<UnitFamily> = Arc::new({
        let a = accessor.clone();
        move || Ok(a.clone())
    });

    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("resource.attributes.length".to_string(), resolver.clone());
    path_resolvers.insert("resource.attributes.name".to_string(), resolver.clone());
    path_resolvers.insert("resource.tags".to_string(), resolver);

    // Test 1: intermediate string key -> integer value
    // resource.attributes["env"].length == 10
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        r#"resource.attributes["env"].length == 10"#,
    );
    assert!(parser.is_error().is_ok(), "Parse should succeed");
    let result = parser.execute(&mut ());
    assert_eq!(result.unwrap(), Value::Bool(true));

    // Test 2: intermediate string key -> string value
    // resource.attributes["env"].name == "production"
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        r#"resource.attributes["env"].name == "production""#,
    );
    assert!(parser.is_error().is_ok(), "Parse should succeed");
    let result = parser.execute(&mut ());
    assert_eq!(result.unwrap(), Value::Bool(true));

    // Test 3: multiple keys on one intermediate field
    // resource.tags["region"]["az"] == "us-east-1a"
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        r#"resource.tags["region"]["az"] == "us-east-1a""#,
    );
    assert!(parser.is_error().is_ok(), "Parse should succeed");
    let result = parser.execute(&mut ());
    assert_eq!(result.unwrap(), Value::Bool(true));

    // Test 4: combined expression using all three paths
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        r#"resource.attributes["env"].length == 10 and resource.attributes["env"].name == "production" and resource.tags["region"]["az"] == "us-east-1a""#,
    );
    assert!(parser.is_error().is_ok(), "Parse should succeed");
    let result = parser.execute(&mut ());
    assert_eq!(result.unwrap(), Value::Bool(true));
}

#[test]
fn produces_expected_field_structure_for_indexed_paths() {
    // Verifies that the parser produces the correct Field structure for paths with
    // per-field indexes. We use a custom accessor that asserts the exact shape of the
    // fields slice it receives.

    #[derive(Debug)]
    struct AssertingAccessor;

    impl PathAccessor<UnitFamily> for AssertingAccessor {
        fn get<'a>(&self, _ctx: &(), fields: &[Field]) -> crate::Result<Value> {
            // For path: ctx.map["key1"]["key2"].leaf
            // Expected fields:
            //   [0] Field { name: "ctx",  keys: [] }
            //   [1] Field { name: "map",  keys: [String("key1"), String("key2")] }
            //   [2] Field { name: "leaf", keys: [] }
            assert_eq!(fields.len(), 3, "Expected 3 fields, got {}", fields.len());

            assert_eq!(fields[0].name, "ctx");
            assert!(fields[0].keys.is_empty(), "ctx should have no keys");

            assert_eq!(fields[1].name, "map");
            assert_eq!(fields[1].keys.len(), 2, "map should have 2 keys");
            match &fields[1].keys[0] {
                IndexExpr::String(s) => assert_eq!(s, "key1"),
                other => panic!("Expected String(\"key1\"), got {:?}", other),
            }
            match &fields[1].keys[1] {
                IndexExpr::String(s) => assert_eq!(s, "key2"),
                other => panic!("Expected String(\"key2\"), got {:?}", other),
            }

            assert_eq!(fields[2].name, "leaf");
            assert!(fields[2].keys.is_empty(), "leaf should have no keys");

            Ok(Value::Bool(true))
        }

        fn set<'a>(&self, _ctx: &mut (), _fields: &[Field], _value: &Value) -> crate::Result<()> {
            Err("not implemented".into())
        }
    }

    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();

    let accessor: Arc<dyn PathAccessor<UnitFamily>> = Arc::new(AssertingAccessor);
    let resolver: PathResolver<UnitFamily> = Arc::new({
        let a = accessor.clone();
        move || Ok(a.clone())
    });

    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("ctx.map.leaf".to_string(), resolver);

    // The expression must evaluate the path, triggering the assertions inside the accessor.
    let parser = Parser::new(
        &editors,
        &converters,
        &enums,
        &path_resolvers,
        r#"ctx.map["key1"]["key2"].leaf"#,
    );
    assert!(parser.is_error().is_ok(), "Parse should succeed");
    let result = parser.execute(&mut ());
    assert_eq!(result.unwrap(), Value::Bool(true), "Accessor assertions should pass");
}

// ============================================================================
// PathAccessor::get tests (path index handling)
// ============================================================================

/// PathAccessor that overrides get: when indexes are present, returns a fixed value
/// so we can verify the evaluator calls get (not only base value + internal index application).
#[derive(Debug)]
struct GetAtOverrideAccessor {
    base_value: Value,
}

impl PathAccessor<UnitFamily> for GetAtOverrideAccessor {
    fn get<'a>(&self, _ctx: &(), fields: &[Field]) -> crate::Result<Value> {
        let has_keys = fields.iter().any(|f| !f.keys.is_empty());
        if !has_keys {
            Ok(self.base_value.clone())
        } else {
            Ok(Value::Int(12345))
        }
    }

    fn set<'a>(&self, _ctx: &mut (), _fields: &[Field], _value: &Value) -> crate::Result<()> {
        Err("set not implemented".into())
    }
}

#[test]
fn indexed_path_access_calls_get() {
    // When a path has indexes (for example, items[0]), the evaluator calls PathAccessor::get.
    // This test uses an accessor that overrides get to return 12345 when indexes are present;
    // we then assert items[0] == 12345 so that the default (get + apply_indexes) is not used.
    let editors = CallbackMap::new();
    let converters = CallbackMap::new();
    let enums = EnumMap::new();
    let accessor = Arc::new(GetAtOverrideAccessor {
        base_value: Value::List(vec![Value::Int(1), Value::Int(2)]),
    });
    let resolver: PathResolver<UnitFamily> =
        Arc::new(move || -> crate::Result<Arc<dyn PathAccessor<UnitFamily>>> { Ok(accessor.clone()) });
    let mut path_resolvers = PathResolverMap::new();
    path_resolvers.insert("items".to_string(), resolver);

    let parser = Parser::new(&editors, &converters, &enums, &path_resolvers, "items[0] == 12345");
    assert!(parser.is_error().is_ok(), "parse error");
    let result = parser.execute(&mut ());
    assert!(result.is_ok(), "execute failed: {:?}", result);
    assert_eq!(
        result.unwrap(),
        Value::Bool(true),
        "get should be called for path with indexes and return 12345"
    );
}
