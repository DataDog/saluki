//! The parser: tokens to a syntax tree whose parameter values are already in the JSON shapes of the IR.
//!
//! Parameter values are converted structurally: a string to a JSON string, a list to an array, a record to an object,
//! `config("b")` to a binding. The operator's parameter schema says only where a predicate is expected, since
//! predicates have their own syntax; it never makes the parser reject a value, which is validation's job.

use std::{collections::BTreeMap, str::FromStr as _};

use saluki_metric_plan::{
    canonical::{PlanHash, BINDING_MEMBER},
    ir::{
        ArtifactNodeId, ArtifactPortRef, BindingName, IdError, InputName, Operand, OperatorName, OutputId, ParamName,
        ParamSpec, ParamType, Params, PortName, ScopeName, BLOB_MEMBER,
    },
    model::{Kind, KindSet},
    operators::OperatorRegistry,
    predicate::{Predicate, SetMatch, StringMatch, TagMatch},
};
use serde_json::{Map, Value};

use crate::{
    diagnostic::{Error, LineIndex, Span},
    graph::{param_specs, positional_predicate},
    lexer::{Token, TokenKind},
};

/// The deepest that lists, records, predicates, and parenthesized expressions may nest.
///
/// This bounds how deeply the parser recurses, so no input can exhaust its stack. A parameter value read from a JSON
/// document nests less deeply than the JSON reader allows, and its text nests no more deeply than its JSON does, so
/// this bound never keeps an artifact read from JSON from printing and parsing back.
pub(crate) const MAX_DEPTH: usize = 128;

/// A parsed policy, before its references are resolved.
pub(crate) struct Policy {
    /// `policy <scope>`.
    pub(crate) head: Span,
    pub(crate) scope: ScopeName,
    pub(crate) ir_version: Option<u32>,
    pub(crate) base: Option<Base>,
    pub(crate) blobs: Option<Map<String, Value>>,
    pub(crate) nodes: Vec<NodeDef>,
    pub(crate) edges: Vec<EdgeStmt>,
    /// The policy's closing brace.
    pub(crate) close: Span,
}

/// The `base` statement.
pub(crate) struct Base {
    pub(crate) program_hash: PlanHash,
    pub(crate) outputs: BTreeMap<OutputId, PlanHash>,
}

/// A `node` definition.
pub(crate) struct NodeDef {
    pub(crate) id: ArtifactNodeId,
    pub(crate) id_span: Span,
    pub(crate) op: OperatorName,
    pub(crate) params: Params,
    /// The span of every parameter value, and of every value nested in one, by parameter path.
    pub(crate) param_spans: Vec<(String, Span)>,
    pub(crate) edges: Vec<BlockEdge>,
    /// The whole definition.
    pub(crate) span: Span,
}

/// One edge line in a node's block.
pub(crate) struct BlockEdge {
    /// The port the line names, if it names one.
    pub(crate) port: Option<(PortName, Span)>,
    pub(crate) target: Target,
    pub(crate) span: Span,
}

/// An edge written as a statement of its own: `input.<name> -> <target>` or `<node>.<port> -> <target>`.
pub(crate) struct EdgeStmt {
    pub(crate) from: ArtifactPortRef,
    pub(crate) from_span: Span,
    pub(crate) target: Target,
    pub(crate) span: Span,
}

/// Where an edge goes.
pub(crate) enum Target {
    /// A node, and the input port if the target names one.
    Node {
        id: ArtifactNodeId,
        port: Option<PortName>,
        span: Span,
    },
    /// A scope input, which is never a valid target; it is accepted so that every artifact can be printed.
    Input { name: InputName, span: Span },
    /// An inline `emit`.
    Emit {
        output: OutputId,
        output_span: Span,
        span: Span,
    },
    /// An inline `drop`.
    Drop { span: Span },
}

impl Target {
    pub(crate) fn span(&self) -> Span {
        match self {
            Self::Node { span, .. } | Self::Input { span, .. } | Self::Emit { span, .. } | Self::Drop { span } => *span,
        }
    }
}

/// Parses a policy from its tokens, which end with [`TokenKind::Eof`].
///
/// # Errors
///
/// Returns the first syntax error, after a note naming the operator when the error is in the arguments of an
/// operator that is not registered: without its schema, the arguments may only be plain values, so a predicate, or a
/// predicate passed without its name, is the likely cause.
pub(crate) fn parse_policy(source: &str, tokens: &[Token]) -> Result<Policy, Vec<Error>> {
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
        depth: 0,
        last_block: None,
        notes: Vec::new(),
    };
    let result = parser.policy().and_then(|policy| {
        if parser.peek().kind != TokenKind::Eof {
            return Err(parser.expected("the end of the input after the policy's closing `}`"));
        }
        Ok(policy)
    });
    result.map_err(|error| {
        let mut errors = std::mem::take(&mut parser.notes);
        errors.push(error);
        errors
    })
}

struct Parser<'s> {
    source: &'s str,
    tokens: &'s [Token],
    pos: usize,
    depth: usize,
    /// The ID of the node whose block was closed last, and the block's opening brace: if the policy is not closed,
    /// that block may be what took its closing brace.
    last_block: Option<(Span, Span)>,
    /// Notes on the likely cause of the syntax error being returned.
    notes: Vec<Error>,
}

// Error messages that refer back to an opening token are only built when there is an error: computing a position
// scans the source, and the recursive functions keep small stack frames when their messages are built elsewhere.
impl<'s> Parser<'s> {
    // Token access. The token list always ends with `Eof`, and the position never moves past it.

    fn peek(&self) -> &'s Token {
        self.peek_at(0)
    }

    fn peek_at(&self, offset: usize) -> &'s Token {
        let tokens: &'s [Token] = self.tokens;
        let last = tokens.len().saturating_sub(1);
        &tokens[(self.pos + offset).min(last)]
    }

    fn advance(&mut self) -> &'s Token {
        let token = self.peek();
        if token.kind != TokenKind::Eof {
            self.pos += 1;
        }
        token
    }

    /// Returns the end of the last token taken.
    fn last_end(&self) -> usize {
        match self.pos.checked_sub(1) {
            Some(last) => self.tokens[last].span.end,
            None => 0,
        }
    }

    fn text(&self, token: &Token) -> &'s str {
        let source: &'s str = self.source;
        &source[token.span.range()]
    }

    fn is_word(&self, token: &Token, word: &str) -> bool {
        token.kind == TokenKind::Ident && self.text(token) == word
    }

    fn peek_is(&self, offset: usize, kind: &TokenKind) -> bool {
        &self.peek_at(offset).kind == kind
    }

    // Errors.

    fn describe(&self, token: &Token) -> String {
        match &token.kind {
            TokenKind::Ident => format!("`{}`", self.text(token)),
            TokenKind::Str(_) => "a string".to_string(),
            TokenKind::Number(_) => format!("the number `{}`", self.text(token)),
            TokenKind::Duration => format!("the duration `{}`", self.text(token)),
            TokenKind::Eof => "the end of the input".to_string(),
            _ => format!("`{}`", self.text(token)),
        }
    }

    /// Returns an error at the next token, saying what was expected instead.
    #[cold]
    fn expected(&self, what: &str) -> Error {
        let token = self.peek();
        Error::new(
            token.span,
            format!("Expected {}, found {}.", what, self.describe(token)),
        )
    }

    /// Returns an error at the next token, saying what was expected instead to continue or close what `open` opened.
    #[cold]
    fn unclosed(&self, what: &str, open: Span) -> Error {
        let (line, column) = LineIndex::new(self.source).line_column(self.source, open.start);
        self.expected(&format!("{} opened at line {}, column {}", what, line, column))
    }

    fn expect(&mut self, kind: TokenKind, what: &str) -> Result<&'s Token, Error> {
        if self.peek().kind == kind {
            Ok(self.advance())
        } else {
            Err(self.expected(what))
        }
    }

    /// Takes the `,` between the items of a delimited sequence and returns `Some(true)`, or returns `Some(false)` and
    /// takes the closing token `close` if it comes instead. Returns `None`, taking nothing, if neither comes.
    fn separator(&mut self, close: &TokenKind) -> Option<bool> {
        let kind = &self.peek().kind;
        if *kind == TokenKind::Comma {
            self.advance();
            Some(true)
        } else if kind == close {
            self.advance();
            Some(false)
        } else {
            None
        }
    }

    /// Takes an identifier and converts it with `make`, reporting the conversion's error at the identifier.
    fn ident<T>(&mut self, what: &str, make: fn(String) -> Result<T, IdError>) -> Result<(T, Span), Error> {
        let token = self.peek();
        if token.kind != TokenKind::Ident {
            return Err(self.expected(what));
        }
        self.advance();
        let value = make(self.text(token).to_string()).map_err(|error| Error::new(token.span, error.to_string()))?;
        Ok((value, token.span))
    }

    /// Takes a port name: an identifier or a decimal index.
    fn port_name(&mut self, what: &str) -> Result<(PortName, Span), Error> {
        let token = self.peek();
        if !matches!(token.kind, TokenKind::Ident | TokenKind::Number(_)) {
            return Err(self.expected(what));
        }
        self.advance();
        let port = PortName::new(self.text(token)).map_err(|error| Error::new(token.span, error.to_string()))?;
        Ok((port, token.span))
    }

    fn string(&mut self, what: &str) -> Result<(String, Span), Error> {
        let token = self.peek();
        match &token.kind {
            TokenKind::Str(value) => {
                self.advance();
                Ok((value.clone(), token.span))
            }
            _ => Err(self.expected(what)),
        }
    }

    fn hash(&mut self, what: &str) -> Result<PlanHash, Error> {
        let (value, span) = self.string(what)?;
        PlanHash::from_str(&value).map_err(|error| Error::new(span, error.to_string()))
    }

    /// Enters one level of nesting at `span`.
    fn enter(&mut self, span: Span) -> Result<(), Error> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(too_deep(span));
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    // The policy.

    fn policy(&mut self) -> Result<Policy, Error> {
        if !self.is_word(self.peek(), "policy") {
            return Err(self.expected("`policy`, which starts a policy artifact"));
        }
        let start = self.advance().span;
        let (scope, scope_span) = self.ident("a scope name after `policy`", ScopeName::new)?;
        let open = self.expect(TokenKind::LBrace, "`{` to open the policy")?.span;

        let mut policy = Policy {
            head: start.to(scope_span),
            scope,
            ir_version: None,
            base: None,
            blobs: None,
            nodes: Vec::new(),
            edges: Vec::new(),
            close: open,
        };
        loop {
            let token = self.peek();
            let next = &self.peek_at(1).kind;
            match &token.kind {
                TokenKind::RBrace => {
                    policy.close = self.advance().span;
                    return Ok(policy);
                }
                TokenKind::Eof => return Err(self.unclosed_policy(open)),
                // A word followed by `.` starts an edge from a node, whatever the word: nodes may be named `node`.
                TokenKind::Ident if *next == TokenKind::Dot => {
                    let edge = self.edge_statement()?;
                    policy.edges.push(edge);
                }
                TokenKind::Ident if *next == TokenKind::Arrow => return Err(self.edge_without_port(token)),
                TokenKind::Ident if self.is_word(token, "ir_version") => {
                    let version = self.ir_version()?;
                    if policy.ir_version.replace(version).is_some() {
                        return Err(Error::new(token.span, "The policy gives `ir_version` twice."));
                    }
                }
                TokenKind::Ident if self.is_word(token, "base") => {
                    let base = self.base()?;
                    if policy.base.replace(base).is_some() {
                        return Err(Error::new(token.span, "The policy gives `base` twice."));
                    }
                }
                TokenKind::Ident if self.is_word(token, "blobs") => {
                    let blobs = self.blobs()?;
                    if policy.blobs.replace(blobs).is_some() {
                        return Err(Error::new(token.span, "The policy gives `blobs` twice."));
                    }
                }
                TokenKind::Ident if self.is_word(token, "node") => {
                    let node = self.node()?;
                    policy.nodes.push(node);
                }
                _ => return Err(self.expected(
                    "`base \"<program hash>\"`, `node <id> = ...`, an edge such as `input.metrics -> <target>`, or \
                         `}` to close the policy",
                )),
            }
        }
    }

    /// Returns the error for a policy that the end of the input leaves open.
    #[cold]
    fn unclosed_policy(&self, open: Span) -> Error {
        let mut error = self.unclosed("`}` to close the policy", open);
        if let Some((node, block)) = self.last_block {
            let (line, column) = LineIndex::new(self.source).line_column(self.source, block.start);
            error.message.push_str(&format!(
                " If a node's block is missing its `}}`, the policy's `}}` closed it instead: check the block of node \
                 `{}` opened at line {}, column {}.",
                &self.source[node.range()],
                line,
                column
            ));
        }
        error
    }

    /// Returns the error for an edge statement whose source, `word`, names no port.
    #[cold]
    fn edge_without_port(&self, word: &Token) -> Error {
        let arrow = self.peek_at(1);
        let message = match self.text(word) {
            "input" => {
                "Expected `.` and the input's name after `input`, as in `input.metrics -> <target>`, found `->`."
                    .to_string()
            }
            node => format!(
                "Expected `.` and a port after `{}`, found `->`: an edge outside a node's block names the port it \
                 leaves, as in `{}.<port> -> <target>`. Or write the edge in the block of node `{}`.",
                node, node, node
            ),
        };
        Error::new(arrow.span, message)
    }

    fn ir_version(&mut self) -> Result<u32, Error> {
        self.advance();
        if let TokenKind::Number(number) = &self.peek().kind {
            if let Some(version) = number.as_u64().and_then(|version| u32::try_from(version).ok()) {
                self.advance();
                return Ok(version);
            }
        }
        Err(self.expected("an IR version, a whole number such as `1`"))
    }

    fn base(&mut self) -> Result<Base, Error> {
        self.advance();
        let program_hash = self.hash("the hash of the base program")?;
        let mut outputs = BTreeMap::new();
        if self.is_word(self.peek(), "outputs") && self.peek_is(1, &TokenKind::LBrace) {
            self.advance();
            let open = self.advance().span;
            loop {
                match self.peek().kind {
                    TokenKind::RBrace => {
                        self.advance();
                        break;
                    }
                    TokenKind::Ident => {}
                    _ => return Err(self.unclosed("an output ID, or `}` to close the outputs", open)),
                }
                let (output, span) = self.ident("an output ID", OutputId::new)?;
                self.expect(TokenKind::Eq, "`=` and the output's fingerprint")?;
                let fingerprint = self.hash("the output's fingerprint, such as \"sha256:<64 hexadecimal digits>\"")?;
                if outputs.insert(output.clone(), fingerprint).is_some() {
                    return Err(Error::new(span, format!("The output `{}` is listed twice.", output)));
                }
                match self.separator(&TokenKind::RBrace) {
                    Some(true) => {}
                    Some(false) => break,
                    None => return Err(self.expected("`,` or `}` after the output's fingerprint")),
                }
            }
        }
        Ok(Base { program_hash, outputs })
    }

    fn blobs(&mut self) -> Result<Map<String, Value>, Error> {
        self.advance();
        let open = self.expect(TokenKind::LBrace, "`{` to open the blob table")?.span;
        let mut blobs = Map::new();
        loop {
            let token = self.peek();
            match token.kind {
                TokenKind::RBrace => {
                    self.advance();
                    return Ok(blobs);
                }
                TokenKind::Str(_) => {}
                _ => return Err(self.unclosed("a blob's hash, or `}` to close the blobs", open)),
            }
            let hash = self.hash("a blob's hash")?;
            self.expect(TokenKind::Eq, "`=` and the blob's value")?;
            let value = self.value(None, "", &mut Vec::new())?;
            if blobs.insert(hash.to_string(), value).is_some() {
                return Err(Error::new(token.span, format!("The blob {} is listed twice.", hash)));
            }
            match self.separator(&TokenKind::RBrace) {
                Some(true) => {}
                Some(false) => return Ok(blobs),
                None => return Err(self.expected("`,` or `}` after the blob's value")),
            }
        }
    }

    fn node(&mut self) -> Result<NodeDef, Error> {
        let start = self.advance().span;
        let (id, id_span) = self.ident("a node ID", ArtifactNodeId::new)?;
        self.expect(TokenKind::Eq, "`=` and the node's operator")?;
        let (op, op_span) = self.ident("an operator name, such as `split` or `tee`", OperatorName::new)?;

        let mut params = Params::new();
        let mut param_spans = Vec::new();
        let tee_sugar = op.as_str() == "tee" && self.peek().kind != TokenKind::LParen;
        if self.peek().kind == TokenKind::LParen {
            if let Err(error) = self.arguments(&op, &mut params, &mut param_spans) {
                if param_specs(op.as_str()).is_none() {
                    self.notes.push(unknown_operator(&op, op_span));
                }
                return Err(error);
            }
        }

        let mut edges = Vec::new();
        let mut block = None;
        if self.peek().kind == TokenKind::LBrace {
            block = Some(self.peek().span);
            edges = self.block(&id, id_span)?;
        }

        if tee_sugar {
            // `tee { -> a -> b }` numbers its ports in order: it is `tee(ports: 2) { 0 -> a 1 -> b }`.
            for (index, edge) in edges.iter_mut().enumerate() {
                if let Some((_, span)) = &edge.port {
                    return Err(Error::new(
                        *span,
                        "A `tee` written without parameters numbers its ports in order, so its lines are written \
                         `-> <target>`. Remove the port name, or write `tee(ports: <n>)` and name every port.",
                    ));
                }
                let port = u16::try_from(index).ok().and_then(PortName::index).ok_or_else(|| {
                    Error::new(
                        edge.span,
                        format!("A `tee` has at most {} ports.", PortName::MAX_INDEX + 1),
                    )
                })?;
                edge.port = Some((port, edge.span));
            }
            let ports = ParamName::new("ports").map_err(|error| Error::new(op_span, error.to_string()))?;
            params.insert(ports, Value::from(edges.len()));
            let span = match block {
                Some(open) => open.to(Span::new(self.last_end(), self.last_end())),
                None => op_span,
            };
            param_spans.push(("ports".to_string(), span));
        }

        Ok(NodeDef {
            id,
            id_span,
            op,
            params,
            param_spans,
            edges,
            span: start.to(Span::new(self.last_end(), self.last_end())),
        })
    }

    fn arguments(
        &mut self, op: &OperatorName, params: &mut Params, spans: &mut Vec<(String, Span)>,
    ) -> Result<(), Error> {
        self.advance();
        let named_next = |parser: &Self| parser.peek().kind == TokenKind::Ident && parser.peek_is(1, &TokenKind::Colon);

        if self.peek().kind != TokenKind::RParen && !named_next(self) {
            if let Some(name) = positional_predicate(op.as_str()) {
                let start = self.peek().span.start;
                let predicate = self.predicate()?;
                let name = ParamName::new(name).map_err(|error| Error::new(self.peek().span, error.to_string()))?;
                spans.push((name.to_string(), Span::new(start, self.last_end())));
                params.insert(name, predicate);
                if self.peek().kind == TokenKind::Comma {
                    self.advance();
                }
                self.expect(TokenKind::RParen, "`and`, `or`, or `)` after the predicate")?;
                return Ok(());
            }
        }

        let specs = param_specs(op.as_str());
        loop {
            if self.peek().kind == TokenKind::RParen {
                self.advance();
                return Ok(());
            }
            if !named_next(self) {
                return Err(self.not_a_parameter(op, specs));
            }
            let (name, name_span) = self.ident("a parameter name", ParamName::new)?;
            self.advance();
            let ty = specs.and_then(|specs| {
                specs
                    .iter()
                    .find(|spec| spec.name == name.as_str())
                    .map(|spec| &spec.ty)
            });
            let value = self.value(ty, name.as_str(), spans)?;
            if params.get(name.as_str()).is_some() {
                return Err(Error::new(
                    name_span,
                    format!("The parameter `{}` is given twice.", name),
                ));
            }
            params.insert(name, value);
            match self.separator(&TokenKind::RParen) {
                Some(true) => {}
                Some(false) => return Ok(()),
                None if matches!(literal_type(ty), Some(ParamType::Predicate)) => {
                    return Err(self.expected("`and`, `or`, `,`, or `)` after the predicate"))
                }
                None => return Err(self.expected("`,` or `)` after the parameter's value")),
            }
        }
    }

    #[cold]
    fn not_a_parameter(&self, op: &OperatorName, specs: Option<&[ParamSpec]>) -> Error {
        let accepted = match specs {
            Some([]) => format!("; `{}` takes no parameters", op),
            Some(specs) => format!(
                "; `{}` takes: {}",
                op,
                specs.iter().map(|spec| spec.name).collect::<Vec<_>>().join(", ")
            ),
            None => String::new(),
        };
        self.expected(&format!("a parameter, written `<name>: <value>`, or `)`{}", accepted))
    }

    fn block(&mut self, node: &ArtifactNodeId, id_span: Span) -> Result<Vec<BlockEdge>, Error> {
        let open = self.advance().span;
        let unclosed = |parser: &Self| parser.unclosed(&format!("`}}` to close the block of node `{}`", node), open);
        let mut edges = Vec::new();
        loop {
            let token = self.peek();
            match &token.kind {
                TokenKind::RBrace => {
                    self.advance();
                    self.last_block = Some((id_span, open));
                    return Ok(edges);
                }
                TokenKind::Eof => return Err(unclosed(self)),
                TokenKind::Ident
                    if self.is_word(token, "node")
                        && self.peek_is(1, &TokenKind::Ident)
                        && self.peek_is(2, &TokenKind::Eq) =>
                {
                    return Err(unclosed(self))
                }
                TokenKind::Arrow => {
                    self.advance();
                    let target = self.target()?;
                    edges.push(BlockEdge {
                        port: None,
                        span: token.span.to(target.span()),
                        target,
                    });
                }
                TokenKind::Ident | TokenKind::Number(_)
                    if matches!(self.peek_at(1).kind, TokenKind::Eq | TokenKind::EqEq) =>
                {
                    self.advance();
                    return Err(self.expected(&format!(
                        "`->` after the port `{}`: edges are written `<port> -> <target>`",
                        self.text(token)
                    )));
                }
                TokenKind::Ident | TokenKind::Number(_) if self.peek_is(1, &TokenKind::Arrow) => {
                    let (port, port_span) = self.port_name("a port name")?;
                    self.advance();
                    let target = self.target()?;
                    edges.push(BlockEdge {
                        port: Some((port, port_span)),
                        span: port_span.to(target.span()),
                        target,
                    });
                }
                _ => {
                    return Err(self.expected(&format!(
                        "an edge such as `true -> <target>` or `-> <target>`, or `}}` to close the block of node `{}`",
                        node
                    )))
                }
            }
        }
    }

    fn edge_statement(&mut self) -> Result<EdgeStmt, Error> {
        let first = self.advance();
        self.advance();
        let (port, port_span) = self.port_name("a port name after `.`")?;
        let from_span = first.span.to(port_span);
        let from = if self.is_word(first, "input") {
            let name = InputName::new(port.as_str()).map_err(|error| Error::new(port_span, error.to_string()))?;
            ArtifactPortRef::Input(name)
        } else {
            let node =
                ArtifactNodeId::new(self.text(first)).map_err(|error| Error::new(first.span, error.to_string()))?;
            ArtifactPortRef::Node { node, port }
        };
        self.expect(TokenKind::Arrow, "`->` and the edge's target")?;
        let target = self.target()?;
        Ok(EdgeStmt {
            from,
            from_span,
            span: from_span.to(target.span()),
            target,
        })
    }

    fn target(&mut self) -> Result<Target, Error> {
        let token = self.peek();
        let what = "a target: a node ID, `emit <output>`, or `drop`";
        if token.kind != TokenKind::Ident {
            return Err(self.expected(what));
        }
        let dotted = self.peek_is(1, &TokenKind::Dot);
        if self.is_word(token, "emit") && !dotted {
            self.advance();
            let (output, output_span) = self.ident("an output ID after `emit`", OutputId::new)?;
            return Ok(Target::Emit {
                output,
                output_span,
                span: token.span.to(output_span),
            });
        }
        if self.is_word(token, "drop") && !dotted {
            self.advance();
            return Ok(Target::Drop { span: token.span });
        }
        if self.is_word(token, "input") && dotted {
            self.advance();
            self.advance();
            let (name, span) = self.ident("a scope input name after `input.`", InputName::new)?;
            return Ok(Target::Input {
                name,
                span: token.span.to(span),
            });
        }
        let (id, span) = self.ident(what, ArtifactNodeId::new)?;
        if !dotted {
            return Ok(Target::Node { id, port: None, span });
        }
        self.advance();
        let (port, port_span) = self.port_name("an input port name after `.`")?;
        Ok(Target::Node {
            id,
            port: Some(port),
            span: span.to(port_span),
        })
    }

    // Values.

    /// Parses a parameter value at `path`, recording the span of it and of every value nested in it into `spans`.
    ///
    /// `ty` is the value's declared type, when the operator's schema declares one.
    fn value(
        &mut self, ty: Option<&'static ParamType>, path: &str, spans: &mut Vec<(String, Span)>,
    ) -> Result<Value, Error> {
        let start = self.peek().span.start;
        let value = if matches!(literal_type(ty), Some(ParamType::Predicate)) {
            self.predicate()?
        } else {
            match self.peek().kind {
                TokenKind::LBracket => self.list(ty, path, spans)?,
                TokenKind::LBrace => self.record(ty, path, spans)?,
                _ => self.scalar()?,
            }
        };
        spans.push((path.to_string(), Span::new(start, self.last_end())));
        Ok(value)
    }

    /// Parses a value that nests no other value.
    fn scalar(&mut self) -> Result<Value, Error> {
        let token = self.peek();
        let value =
            match &token.kind {
                TokenKind::Str(value) => Value::String(value.clone()),
                TokenKind::Number(number) => Value::Number(number.clone()),
                TokenKind::Duration => Value::String(self.text(token).to_string()),
                TokenKind::Ident => match self.text(token) {
                    "true" => Value::Bool(true),
                    "false" => Value::Bool(false),
                    "null" => Value::Null,
                    "config" if self.peek_is(1, &TokenKind::LParen) => {
                        let binding = self.config()?;
                        return Ok(single_member(BINDING_MEMBER, Value::String(binding.to_string())));
                    }
                    "blob" if self.peek_is(1, &TokenKind::LParen) => {
                        self.advance();
                        self.advance();
                        let hash = self.hash("the blob's hash, such as \"sha256:<64 hexadecimal digits>\"")?;
                        self.expect(TokenKind::RParen, "`)` after the blob's hash")?;
                        return Ok(single_member(BLOB_MEMBER, Value::String(hash.to_string())));
                    }
                    word => return Err(bare_word(token.span, word)),
                },
                _ => return Err(self.expected(
                    "a value: a string, a number, a duration such as `10s`, `true`, `false`, `null`, a list `[...]`, \
                     a record `{...}`, `config(\"<binding>\")`, or `blob(\"<hash>\")`",
                )),
            };
        self.advance();
        Ok(value)
    }

    fn list(
        &mut self, ty: Option<&'static ParamType>, path: &str, spans: &mut Vec<(String, Span)>,
    ) -> Result<Value, Error> {
        let open = self.advance().span;
        self.enter(open)?;
        let element = match literal_type(ty) {
            Some(ParamType::List(element) | ParamType::Set(element)) => Some(*element),
            _ => None,
        };
        let mut values = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::RBracket => {
                    self.advance();
                    break;
                }
                TokenKind::Eof => return Err(self.unclosed("`]` to close the list", open)),
                _ => {}
            }
            let value = self.value(element, &format!("{}[{}]", path, values.len()), spans)?;
            values.push(value);
            match self.separator(&TokenKind::RBracket) {
                Some(true) => {}
                Some(false) => break,
                None => {
                    return Err(self.unclosed("`,` or `]` after the list element, to continue or close the list", open))
                }
            }
        }
        self.leave();
        Ok(Value::Array(values))
    }

    fn record(
        &mut self, ty: Option<&'static ParamType>, path: &str, spans: &mut Vec<(String, Span)>,
    ) -> Result<Value, Error> {
        let open = self.advance().span;
        self.enter(open)?;
        let mut members = Map::new();
        loop {
            let token = self.peek();
            let key = match &token.kind {
                TokenKind::RBrace => {
                    self.advance();
                    break;
                }
                TokenKind::Ident => self.text(token).to_string(),
                TokenKind::Str(key) => key.clone(),
                _ => return Err(self.unclosed("a member name, or `}` to close the record", open)),
            };
            self.advance();
            self.expect(TokenKind::Colon, "`:` and the member's value")?;
            let member_type = match literal_type(ty) {
                Some(ParamType::Record(specs)) => specs.iter().find(|spec| spec.name == key).map(|spec| &spec.ty),
                Some(ParamType::StringMap(element)) => Some(*element),
                _ => None,
            };
            if members.contains_key(&key) {
                return Err(repeated_member(token.span, &key));
            }
            let member_path = if path.is_empty() {
                key.clone()
            } else {
                format!("{}.{}", path, key)
            };
            let value = self.value(member_type, &member_path, spans)?;
            members.insert(key, value);
            match self.separator(&TokenKind::RBrace) {
                Some(true) => {}
                Some(false) => break,
                None => {
                    return Err(self.unclosed(
                        "`,` or `}` after the member's value, to continue or close the record",
                        open,
                    ))
                }
            }
        }
        self.leave();
        Ok(Value::Object(members))
    }

    /// Parses `config("<binding>")`.
    fn config(&mut self) -> Result<BindingName, Error> {
        self.advance();
        self.advance();
        let (name, span) = self.string("the binding's name, such as \"dogstatsd.metric_filter.values\"")?;
        let binding = BindingName::new(name).map_err(|error| Error::new(span, error.to_string()))?;
        self.expect(TokenKind::RParen, "`)` after the binding's name")?;
        Ok(binding)
    }

    // Predicates.

    /// Parses a predicate: `or` of `and` of possibly negated primaries.
    fn predicate(&mut self) -> Result<Value, Error> {
        self.enter(self.peek().span)?;
        let mut operands = vec![self.conjunction()?];
        while self.is_word(self.peek(), "or") {
            self.advance();
            operands.push(self.conjunction()?);
        }
        self.leave();
        Ok(junction("any", operands))
    }

    fn conjunction(&mut self) -> Result<Value, Error> {
        let mut operands = vec![self.negation()?];
        while self.is_word(self.peek(), "and") {
            self.advance();
            operands.push(self.negation()?);
        }
        Ok(junction("all", operands))
    }

    fn negation(&mut self) -> Result<Value, Error> {
        let mut negations = 0;
        while self.is_word(self.peek(), "not") {
            self.enter(self.peek().span)?;
            self.advance();
            negations += 1;
        }
        let mut predicate = self.primary()?;
        for _ in 0..negations {
            predicate = single_member("not", predicate);
            self.leave();
        }
        Ok(predicate)
    }

    fn primary(&mut self) -> Result<Value, Error> {
        let token = self.peek();
        let call = self.peek_is(1, &TokenKind::LParen);
        match &token.kind {
            TokenKind::LParen => {
                let open = self.advance().span;
                let predicate = self.predicate()?;
                if self.peek().kind != TokenKind::RParen {
                    return Err(self.unclosed("`and`, `or`, or `)` to close the parenthesis", open));
                }
                self.advance();
                Ok(predicate)
            }
            TokenKind::LBrace | TokenKind::LBracket | TokenKind::Str(_) | TokenKind::Number(_) => self.raw(),
            TokenKind::Ident => match self.text(token) {
                "null" => self.raw(),
                "config" | "blob" if call => self.raw(),
                word @ ("all" | "any") if call => {
                    self.advance();
                    let operands = self.predicate_arguments()?;
                    Ok(single_member(word, Value::Array(operands)))
                }
                _ => self.leaf(),
            },
            _ => Err(self.expected(PREDICATE_EXPECTED)),
        }
    }

    /// Parses a value written raw where a predicate is expected.
    fn raw(&mut self) -> Result<Value, Error> {
        self.value(None, "", &mut Vec::new())
    }

    /// Parses a predicate leaf.
    fn leaf(&mut self) -> Result<Value, Error> {
        let token = self.peek();
        let call = self.peek_is(1, &TokenKind::LParen);
        let leaf = match self.text(token) {
            "true" | "false" => {
                self.advance();
                Predicate::Const(self.text(token) == "true")
            }
            "name" => {
                self.advance();
                Predicate::Name(self.string_match()?)
            }
            "intake_name" => {
                self.advance();
                Predicate::IntakeName(self.string_match()?)
            }
            "host" if self.is_word(self.peek_at(1), "is") => {
                self.advance();
                self.advance();
                if !self.is_word(self.peek(), "unset") {
                    return Err(self.expected("`unset` after `host is`"));
                }
                self.advance();
                Predicate::HostUnset {}
            }
            "host" => {
                self.advance();
                Predicate::Host(self.string_match()?)
            }
            "has_tag" if call => Predicate::HasTag(self.string_argument("the tag key")?),
            "has_origin_tag" if call => Predicate::HasOriginTag(self.string_argument("the origin tag key")?),
            "tag" if call => Predicate::Tag(self.tag_match()?),
            "origin_tag" if call => Predicate::OriginTag(self.tag_match()?),
            "kind" => {
                self.advance();
                if !self.is_word(self.peek(), "in") {
                    return Err(self.expected("`in` and a list of kinds after `kind`"));
                }
                self.advance();
                Predicate::KindIn(self.kinds()?)
            }
            "flag" if call => {
                self.advance();
                self.advance();
                let flag = self.bool_operand()?;
                self.expect(TokenKind::RParen, "`)` after the flag")?;
                Predicate::Flag(flag)
            }
            "is_empty" if call => {
                self.advance();
                self.advance();
                let list = self.strings_operand()?;
                self.expect(TokenKind::RParen, "`)` after the list")?;
                Predicate::IsEmpty(list)
            }
            word => {
                if let Some(example) = call_example(word) {
                    self.advance();
                    return Err(self.expected(&format!("`(` after `{}`, as in `{}`", word, example)));
                }
                return Err(self.expected(PREDICATE_EXPECTED));
            }
        };
        serde_json::to_value(&leaf).map_err(|error| Error::new(token.span, format!("Invalid predicate: {}", error)))
    }

    /// Parses the parenthesized, comma-separated predicates of `all(...)` or `any(...)`.
    fn predicate_arguments(&mut self) -> Result<Vec<Value>, Error> {
        let open = self.advance().span;
        let mut operands = Vec::new();
        loop {
            if self.peek().kind == TokenKind::RParen {
                self.advance();
                return Ok(operands);
            }
            operands.push(self.predicate()?);
            match self.separator(&TokenKind::RParen) {
                Some(true) => {}
                Some(false) => return Ok(operands),
                None => {
                    return Err(self.unclosed(
                        "`and`, `or`, `,`, or `)` after the predicate, to continue or close the list",
                        open,
                    ))
                }
            }
        }
    }

    /// Parses `("<string>")`, after the word before it.
    fn string_argument(&mut self, what: &str) -> Result<String, Error> {
        self.advance();
        self.advance();
        if !matches!(self.peek().kind, TokenKind::Str(_)) {
            return Err(self.expected(&format!("{} as a string", what)));
        }
        let (value, _) = self.string(what)?;
        if self.peek().kind != TokenKind::RParen {
            return Err(self.expected(&format!("`)` after {}", what)));
        }
        self.advance();
        Ok(value)
    }

    fn tag_match(&mut self) -> Result<TagMatch, Error> {
        let key = self.string_argument("the tag key")?;
        let value = self.string_match()?;
        Ok(TagMatch { key, value })
    }

    fn string_match(&mut self) -> Result<StringMatch, Error> {
        let token = self.peek();
        if token.kind == TokenKind::EqEq {
            self.advance();
            return Ok(StringMatch::Eq(self.string("a string after `==`")?.0));
        }
        let word = if token.kind == TokenKind::Ident {
            self.text(token)
        } else {
            ""
        };
        match word {
            "starts_with" => {
                self.advance();
                Ok(StringMatch::Prefix(self.string("a string after `starts_with`")?.0))
            }
            "in" => {
                self.advance();
                Ok(StringMatch::In(self.strings()?))
            }
            "prefix_in" => {
                self.advance();
                Ok(StringMatch::PrefixIn(self.strings()?))
            }
            "matches" if self.peek_is(1, &TokenKind::LParen) => {
                self.advance();
                self.set_match().map(StringMatch::Matches)
            }
            "matches" => {
                self.advance();
                Err(self.expected("`(` after `matches`, as in `matches(values: [\"a\"], prefix: false)`"))
            }
            _ => Err(self.expected(
                "a string match: `== \"<s>\"`, `starts_with \"<s>\"`, `in [...]`, `prefix_in [...]`, or \
                 `matches(values: ..., prefix: ...)`",
            )),
        }
    }

    /// Parses `(values: <list or binding>, prefix: <bool or binding>)`, in either order.
    fn set_match(&mut self) -> Result<SetMatch, Error> {
        let open = self.advance().span;
        let mut values = None;
        let mut prefix = None;
        let close = loop {
            if self.peek().kind == TokenKind::RParen {
                break self.advance().span;
            }
            let token = self.peek();
            let word = if token.kind == TokenKind::Ident && self.peek_is(1, &TokenKind::Colon) {
                self.text(token)
            } else {
                ""
            };
            let repeated = match word {
                "values" => {
                    self.advance();
                    self.advance();
                    values.replace(self.strings_operand()?).is_some()
                }
                "prefix" => {
                    self.advance();
                    self.advance();
                    prefix.replace(self.bool_operand()?).is_some()
                }
                _ => return Err(self.expected("`values:` or `prefix:`")),
            };
            if repeated {
                return Err(Error::new(token.span, format!("`{}` is given twice.", word)));
            }
            match self.separator(&TokenKind::RParen) {
                Some(true) => {}
                Some(false) => break self.tokens[self.pos - 1].span,
                None => return Err(self.expected("`,` or `)` after the argument")),
            }
        };
        match (values, prefix) {
            (Some(values), Some(prefix)) => Ok(SetMatch { values, prefix }),
            (values, _) => Err(Error::new(
                open.to(close),
                format!(
                    "`matches` needs both `values:` and `prefix:`; `{}:` is missing.",
                    if values.is_none() { "values" } else { "prefix" }
                ),
            )),
        }
    }

    /// Parses a list of string literals.
    fn strings(&mut self) -> Result<Vec<String>, Error> {
        let open = self
            .expect(TokenKind::LBracket, "a list of strings, such as `[\"a\", \"b\"]`")?
            .span;
        let mut values = Vec::new();
        loop {
            let token = self.peek();
            match &token.kind {
                TokenKind::RBracket => {
                    self.advance();
                    return Ok(values);
                }
                TokenKind::Str(value) => {
                    self.advance();
                    values.push(value.clone());
                }
                _ => return Err(self.unclosed("a string, or `]` to close the list", open)),
            }
            match self.separator(&TokenKind::RBracket) {
                Some(true) => {}
                Some(false) => return Ok(values),
                None => return Err(self.unclosed("`,` or `]` after the string, to continue or close the list", open)),
            }
        }
    }

    fn strings_operand(&mut self) -> Result<Operand<Vec<String>>, Error> {
        if self.is_word(self.peek(), "config") && self.peek_is(1, &TokenKind::LParen) {
            return self.config().map(Operand::config);
        }
        if self.peek().kind != TokenKind::LBracket {
            return Err(self.expected("a list of strings, or `config(\"<binding>\")`"));
        }
        self.strings().map(Operand::literal)
    }

    fn bool_operand(&mut self) -> Result<Operand<bool>, Error> {
        let token = self.peek();
        if self.is_word(token, "config") && self.peek_is(1, &TokenKind::LParen) {
            return self.config().map(Operand::config);
        }
        if self.is_word(token, "true") || self.is_word(token, "false") {
            self.advance();
            return Ok(Operand::literal(self.text(token) == "true"));
        }
        Err(self.expected("`true`, `false`, or `config(\"<binding>\")`"))
    }

    fn kinds(&mut self) -> Result<KindSet, Error> {
        let open = self
            .expect(TokenKind::LBracket, "a list of kinds, such as `[counter, gauge]`")?
            .span;
        let mut kinds = KindSet::EMPTY;
        loop {
            let token = self.peek();
            match token.kind {
                TokenKind::RBracket => {
                    self.advance();
                    return Ok(kinds);
                }
                TokenKind::Ident => {}
                _ => return Err(self.expected(&format!("a metric kind ({}), or `]`", kind_names()))),
            }
            let Some(kind) = Kind::ALL.into_iter().find(|kind| kind.as_str() == self.text(token)) else {
                return Err(Error::new(
                    token.span,
                    format!(
                        "Unknown metric kind `{}`; the kinds are {}.",
                        self.text(token),
                        kind_names()
                    ),
                ));
            };
            if kinds.contains(kind) {
                return Err(Error::new(
                    token.span,
                    format!("The kind `{}` is listed twice.", kind.as_str()),
                ));
            }
            self.advance();
            kinds = kinds.with(kind);
            match self.separator(&TokenKind::RBracket) {
                Some(true) => {}
                Some(false) => return Ok(kinds),
                None => return Err(self.unclosed("`,` or `]` after the kind, to continue or close the list", open)),
            }
        }
    }
}

const PREDICATE_EXPECTED: &str =
    "a predicate, such as `name == \"requests\"`, `has_tag(\"env\")`, `kind in [counter]`, \
                                  `true`, `not <predicate>`, or `(<predicate>)`";

/// Returns an example of the predicate that starts with `word` and takes parentheses, if one does.
fn call_example(word: &str) -> Option<&'static str> {
    Some(match word {
        "has_tag" => "has_tag(\"env\")",
        "has_origin_tag" => "has_origin_tag(\"pod\")",
        "tag" => "tag(\"team\") == \"platform\"",
        "origin_tag" => "origin_tag(\"pod\") starts_with \"web-\"",
        "flag" => "flag(config(\"multi_region_failover.enabled\"))",
        "is_empty" => "is_empty([])",
        "all" => "all(<predicate>, ...)",
        "any" => "any(<predicate>, ...)",
        _ => return None,
    })
}

/// Returns the note on an operator that is not registered, whose arguments did not parse.
#[cold]
fn unknown_operator(op: &OperatorName, span: Span) -> Error {
    let registry = OperatorRegistry::native();
    let names = registry.iter().map(|def| def.name()).collect::<Vec<_>>();
    let suggestion = closest(op.as_str(), &names)
        .map(|name| format!(" Did you mean `{}`?", name))
        .unwrap_or_default();
    Error::new(
        span,
        format!(
            "`{}` is not a registered operator, so its arguments can only be plain values, written \
             `<name>: <value>`.{} The operators are {}.",
            op,
            suggestion,
            names.join(", ")
        ),
    )
}

/// Returns the name in `names` closest to `word`, if one is within a third of its length in edits.
fn closest<'n>(word: &str, names: &[&'n str]) -> Option<&'n str> {
    let limit = (word.chars().count() / 3).max(1);
    names
        .iter()
        .map(|name| (edit_distance(word, name), *name))
        .filter(|(distance, _)| *distance <= limit)
        .min()
        .map(|(_, name)| name)
}

/// Returns the number of single-character insertions, deletions, and substitutions that turn `a` into `b`.
fn edit_distance(a: &str, b: &str) -> usize {
    let b = b.chars().collect::<Vec<_>>();
    let mut previous = (0..=b.len()).collect::<Vec<_>>();
    for (i, a) in a.chars().enumerate() {
        let mut current = vec![i + 1; b.len() + 1];
        for (j, b) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(a != *b);
            current[j + 1] = substitution.min(previous[j + 1] + 1).min(current[j] + 1);
        }
        previous = current;
    }
    previous[b.len()]
}

#[cold]
fn too_deep(span: Span) -> Error {
    Error::new(
        span,
        format!(
            "This nests more than {} levels deep, the most a policy may. Flatten the value or predicate.",
            MAX_DEPTH
        ),
    )
}

#[cold]
fn bare_word(span: Span, word: &str) -> Error {
    Error::new(
        span,
        format!(
            "Expected a value, found `{}`. Strings are written in double quotes, as \"{}\".",
            word, word
        ),
    )
}

#[cold]
fn repeated_member(span: Span, key: &str) -> Error {
    Error::new(span, format!("The member `{}` is given twice.", key))
}

fn kind_names() -> String {
    Kind::ALL
        .iter()
        .map(|kind| kind.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Returns a parameter type with any configuration binding unwrapped: the type its literal values have.
pub(crate) fn literal_type(ty: Option<&'static ParamType>) -> Option<&'static ParamType> {
    match ty {
        Some(ParamType::Bindable(inner)) => Some(*inner),
        ty => ty,
    }
}

fn single_member(name: &str, value: Value) -> Value {
    let mut members = Map::new();
    members.insert(name.to_string(), value);
    Value::Object(members)
}

/// Returns the junction `name` of `operands`, or the only operand when there is one.
fn junction(name: &str, mut operands: Vec<Value>) -> Value {
    if operands.len() == 1 {
        operands.remove(0)
    } else {
        single_member(name, Value::Array(operands))
    }
}
