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
        accessor.get(&*ctx)?;
        accessor.set(ctx)?;
        Ok(())
    }
}

// =============================================================================
// Context and PathAccessor implementation for the sandbox
// =============================================================================

/// Context type for the sandbox (holds a vector of integers).
#[derive(Debug, Default)]
pub struct Ctx {
    /// Vector of integers stored in the context.
    pub numbers: Vec<i64>,
}

/// PathAccessor implementation that does nothing (for sandbox).
#[derive(Debug)]
pub struct EmptyPathAccessor;

impl PathAccessor<Ctx> for EmptyPathAccessor {
    fn get(&self, ctx: &Ctx) -> Result<()> {
        println!("ctx.numbers = {:?}", ctx.numbers);
        Ok(())
    }

    fn set(&self, _ctx: &mut Ctx) -> Result<()> {
        Ok(())
    }
}

fn main() {
    let mut path_resolver_map: PathResolverMap<Ctx> = HashMap::new();
    path_resolver_map.insert(
        "attr".to_string(),
        Arc::new(|| Ok(Arc::new(EmptyPathAccessor) as Arc<dyn PathAccessor<Ctx> + Send + Sync>)),
    );

    let parser = Parser::<Ctx>::new(path_resolver_map);
    let mut ctx = Ctx {
        numbers: vec![1, 2, 3],
    };

    if let Err(e) = parser.execute(&mut ctx, "attr") {
        eprintln!("execute error: {}", e);
        std::process::exit(1);
    }

    println!("Привет, мир!");
}
