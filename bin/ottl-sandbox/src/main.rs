use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

/// Standard error type (boxed trait object).
pub type BoxError = Box<dyn Error + Send + Sync>;

/// Standard result type for the library
pub type Result<T> = std::result::Result<T, BoxError>;

pub trait PathAccessor<C>: fmt::Debug {
    fn get(&self, ctx: &C) -> Result<()>;
    fn set(&self, ctx: &mut C) -> Result<()>;
}

/// Type alias for the path resolver function for context type `C`.
/// Returns a [`PathAccessor`] that operates on context `C`.
pub type PathResolver<C> = Arc<dyn Fn() -> Result<Arc<dyn PathAccessor<C> + Send + Sync>> + Send + Sync>;

/// Map from path string to its resolver. Parser looks up each path in the expression
/// in this map; if a path is missing, parsing fails with an error.
pub type PathResolverMap<C> = HashMap<String, PathResolver<C>>;

pub trait OttlParser<C> {
    fn is_error(&self) -> Result<()>;
    fn execute(&self, ctx: &mut C, path: &str) -> Result<()>;
}

/// Parser that holds a [`PathResolverMap`] and implements [`OttlParser`].
pub struct Parser<C> {
    path_resolver_map: PathResolverMap<C>,
}

impl<C> Parser<C> {
    /// Creates a new parser that uses the given path resolver map.
    pub fn new(path_resolver_map: PathResolverMap<C>) -> Self {
        Self { path_resolver_map }
    }
}

impl<C> OttlParser<C> for Parser<C> {
    fn is_error(&self) -> Result<()> {
        Ok(())
    }

    fn execute(&self, ctx: &mut C, path: &str) -> Result<()> {
        let resolver = self.path_resolver_map.get(path).ok_or_else(|| {
            let msg: BoxError = format!("path not found: {}", path).into();
            msg
        })?;
        let accessor = resolver()?;
        accessor.set(ctx)?;
        accessor.get(&*ctx)?;
        Ok(())
    }
}

// =============================================================================
// Context and PathAccessor implementation for the sandbox
// =============================================================================

/// Context type for the sandbox (holds a vector of integers and a reference to one).
#[derive(Debug)]
pub struct Ctx<'a> {
    /// Vector of integers stored in the context.
    pub numbers: Vec<i64>,
    /// Reference to a vector of integers.
    pub numbers_ref: &'a mut Vec<i64>,
}

/// PathAccessor implementation that does nothing (for sandbox).
#[derive(Debug)]
pub struct EmptyPathAccessor;

impl<'a> PathAccessor<Ctx<'a>> for EmptyPathAccessor {
    fn get(&self, ctx: &Ctx<'a>) -> Result<()> {
        println!(
            "ctx.numbers = [{}]",
            ctx.numbers
                .iter()
                .map(|n| format!("0x{:x}", n))
                .collect::<Vec<_>>()
                .join(", ")
        );
        println!(
            "ctx.numbers_ref = [{}]",
            ctx.numbers_ref
                .iter()
                .map(|n| format!("0x{:x}", n))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(())
    }

    fn set(&self, ctx: &mut Ctx<'a>) -> Result<()> {
        ctx.numbers.push(0xDEADBEEF);
        ctx.numbers_ref.push(0xDEADBEEF);
        Ok(())
    }
}

fn parser_exec<'a>(parser: &Parser<Ctx<'a>>, extra: &'a mut Vec<i64>) {
    let mut ctx = Ctx {
        numbers: vec![1, 2, 3],
        numbers_ref: extra,
    };

    if let Err(e) = parser.execute(&mut ctx, "attr") {
        eprintln!("execute error: {}", e);
        std::process::exit(1);
    }
}

fn main() {
    let mut extra = vec![10, 20, 30];

    let mut path_resolver_map: PathResolverMap<Ctx<'_>> = HashMap::new();
    path_resolver_map.insert(
        "attr".to_string(),
        Arc::new(|| Ok(Arc::new(EmptyPathAccessor) as Arc<dyn PathAccessor<Ctx<'_>> + Send + Sync>)),
    );

    let parser = Parser::<Ctx<'_>>::new(path_resolver_map);
    parser_exec(&parser, &mut extra);

    println!("Пот бережет кровь!");
}
