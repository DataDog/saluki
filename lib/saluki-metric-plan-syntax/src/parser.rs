//! The parser: tokens to a [syntax tree](crate::ast), whose parameter values and predicates are already in the JSON
//! shapes of the IR.
//!
//! Parameter values are converted structurally: a string to a JSON string, a list to an array, a record to an object,
//! `config.a.b` to a binding. The operator's parameter schema says only which parameter may be passed first without its
//! name and where a predicate is expected, since predicates have their own syntax; it never makes the parser reject a
//! value, which is validation's job.
//!
//! The grammar has no separators between statements, so three rules keep it unambiguous:
//!
//! - An operator's arguments open with a `(` written right after its name, as in `aggregate(window: 10s)`. A `(` after
//!   a space starts a tuple: the head of the next statement.
//! - After `emit`, the word `output` always starts `emit output <output>`. A recipient named `output` or `as` is
//!   quoted, as in `emit "output"`.
//! - A word that could start a header (`ir_version`, `base`, `blobs`) or a `let` names a stream instead when `|` or `.`
//!   follows it.

use std::{collections::BTreeMap, str::FromStr as _};

use saluki_metric_plan::{
    canonical::{PlanHash, BINDING_MEMBER},
    ir::{
        ArtifactNodeId, BindingName, ExitName, IdError, Operand, OperatorName, OutputId, ParamName, ParamSpec,
        ParamType, Params, PortName, ScopeName, BLOB_MEMBER,
    },
    model::{Kind, KindSet},
    operators::OperatorRegistry,
    predicate::{Predicate, SetMatch, StringMatch, TagMatch},
};
use serde_json::{Map, Value};

use crate::{
    ast::{
        Base, Branch, End, Fork, Head, Name, OpStage, Pipeline, Program, Ref, Sink, SinkKind, Stage, Statement, Tuple,
        WhereStage,
    },
    diagnostic::{Error, LineIndex, Span},
    graph::param_specs,
    lexer::{Token, TokenKind},
};

/// The deepest that lists, records, predicates, parenthesized expressions, and forks may nest.
///
/// This bounds how deeply the parser recurses, so no input can exhaust its stack. A parameter value read from a JSON
/// document nests less deeply than the JSON reader allows, and its text nests no more deeply than its JSON does, so
/// this bound never keeps an artifact read from JSON from printing and parsing back.
pub(crate) const MAX_DEPTH: usize = 128;

/// Parses a program from its tokens, which end with [`TokenKind::Eof`](crate::lexer::TokenKind::Eof).
///
/// # Errors
///
/// Returns the first syntax error, after a note naming the operator when the error is in the arguments of an
/// operator that is not registered: without its schema, the arguments may only be plain values, so a predicate, or a
/// value passed without its name, is the likely cause.
pub(crate) fn parse_program(source: &str, tokens: &[Token]) -> Result<Program, Vec<Error>> {
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
        depth: 0,
        last_fork: None,
        notes: Vec::new(),
    };
    let result = parser.program().and_then(|program| {
        if parser.peek().kind != TokenKind::Eof {
            return Err(parser.expected("the end of the input after the policy's closing `}`"));
        }
        Ok(program)
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
    /// The opening brace of the fork closed last: if the policy is not closed, that fork may be what took its closing
    /// brace.
    last_fork: Option<Span>,
    /// Notes on the likely cause of the syntax error being returned.
    notes: Vec<Error>,
}

/// What follows a `|`: a stage, or the end of the pipeline.
enum Item {
    Stage(Stage),
    End(End),
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

    /// Returns the span from `start` to the end of the last token taken.
    fn since(&self, start: Span) -> Span {
        Span::new(start.start, self.last_end().max(start.start))
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

    /// Returns `true` if the token at `offset` is followed by `|` or `.`, which makes a word a stream's name whatever
    /// the word.
    fn names_stream(&self, offset: usize) -> bool {
        matches!(self.peek_at(offset + 1).kind, TokenKind::Pipe | TokenKind::Dot)
    }

    /// Returns `true` if a line break comes before the token at `offset`.
    fn starts_line(&self, offset: usize) -> bool {
        let at = self.pos + offset;
        let start = self.peek_at(offset).span.start;
        let previous = match at.checked_sub(1).and_then(|previous| self.tokens.get(previous)) {
            Some(previous) => previous.span.end,
            None => 0,
        };
        self.source
            .get(previous.min(start)..start)
            .is_some_and(|gap| gap.contains('\n'))
    }

    /// Returns `true` if the token at `offset` is followed right away, without a space, by `(`: an operator and its
    /// arguments.
    fn is_call(&self, offset: usize) -> bool {
        let next = self.peek_at(offset + 1);
        next.kind == TokenKind::LParen && next.span.start == self.peek_at(offset).span.end
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

    /// Takes a node ID or a stream's name.
    fn name(&mut self, what: &str) -> Result<Name, Error> {
        let (id, span) = self.ident(what, ArtifactNodeId::new)?;
        Ok(Name { id, span })
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

    // The program and its headers.

    fn program(&mut self) -> Result<Program, Error> {
        if !self.is_word(self.peek(), "policy") {
            return Err(self.expected("`policy`, which starts a policy artifact"));
        }
        let start = self.advance().span;
        let (scope, scope_span) = self.ident("a scope name after `policy`", ScopeName::new)?;
        let open = self.expect(TokenKind::LBrace, "`{` to open the policy")?.span;

        let mut program = Program {
            head: start.to(scope_span),
            scope,
            ir_version: None,
            base: None,
            blobs: None,
            statements: Vec::new(),
            close: open,
        };
        loop {
            let token = self.peek();
            match &token.kind {
                TokenKind::RBrace => {
                    program.close = self.advance().span;
                    return Ok(program);
                }
                TokenKind::Eof => return Err(self.unclosed_policy(open)),
                TokenKind::Ident
                    if matches!(self.text(token), "ir_version" | "base" | "blobs") && !self.names_stream(0) =>
                {
                    if !program.statements.is_empty() {
                        return Err(Error::new(
                            token.span,
                            format!(
                                "`{}` comes before the policy's statements: move it to the top of the policy.",
                                self.text(token)
                            ),
                        ));
                    }
                    let repeated = match self.text(token) {
                        "ir_version" => {
                            let version = self.ir_version()?;
                            program.ir_version.replace(version).is_some()
                        }
                        "base" => {
                            let base = self.base()?;
                            program.base.replace(base).is_some()
                        }
                        _ => {
                            let blobs = self.blobs()?;
                            program.blobs.replace(blobs).is_some()
                        }
                    };
                    if repeated {
                        return Err(Error::new(
                            token.span,
                            format!("The policy gives `{}` twice.", self.text(token)),
                        ));
                    }
                }
                _ => {
                    let statement = self.statement()?;
                    program.statements.push(statement);
                }
            }
        }
    }

    /// Returns the error for a policy that the end of the input leaves open.
    #[cold]
    fn unclosed_policy(&self, open: Span) -> Error {
        let mut error = self.unclosed("`}` to close the policy", open);
        if let Some(fork) = self.last_fork {
            let (line, column) = LineIndex::new(self.source).line_column(self.source, fork.start);
            error.message.push_str(&format!(
                " If a fork is missing its `}}`, the policy's `}}` closed it instead: check the fork opened at line \
                 {}, column {}.",
                line, column
            ));
        }
        error
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

    // Statements and pipelines.

    fn statement(&mut self) -> Result<Statement, Error> {
        let token = self.peek();
        if self.is_word(token, "let") && !self.names_stream(0) {
            let start = self.advance().span;
            let name = self.name("a name for the stream after `let`")?;
            self.expect(TokenKind::Eq, "`=` and the stream's pipeline after the name")?;
            let pipeline = self.pipeline(Some(&name))?;
            self.missing_pipe(&pipeline)?;
            let span = start.to(pipeline.span);
            return Ok(Statement::Let { name, pipeline, span });
        }

        let pipeline = self.pipeline(None)?;
        if pipeline.end.is_none() {
            self.missing_pipe(&pipeline)?;
            return Err(Error::new(
                pipeline.span,
                "This pipeline does not end: a statement without `let` ends in a sink (`emit`, `drop`, or `exit`) or a \
                 `fork`. Name the stream with `let <name> = ...` to use it in later statements, or end it, as with \
                 `| drop`.",
            ));
        }
        let span = pipeline.span;
        Ok(Statement::Run { pipeline, span })
    }

    /// Parses a pipeline. In a `let`, `name` is the stream's name, and the pipeline cannot end.
    fn pipeline(&mut self, name: Option<&Name>) -> Result<Pipeline, Error> {
        let start = self.peek().span;
        let head = self.head()?;
        let mut stages = Vec::new();
        let mut end = None;
        loop {
            match self.peek().kind {
                TokenKind::Pipe => {}
                TokenKind::Arrow => return Err(arrow(self.peek().span)),
                _ => break,
            }
            self.advance();
            match self.item(name, "a stage after `|`")? {
                Item::Stage(stage) => stages.push(stage),
                Item::End(last) => {
                    if self.peek().kind == TokenKind::Pipe {
                        return Err(self.end_in_middle(&last, false));
                    }
                    end = Some(last);
                    break;
                }
            }
        }
        let next = self.peek();
        if next.kind == TokenKind::Ident
            && matches!(self.text(next), "emit" | "drop" | "exit" | "fork" | "where")
            && !self.names_stream(0)
        {
            return Err(Error::new(
                next.span,
                format!(
                    "Expected `|` before `{}`, which is a stage: it follows a stream and `|`, as in `dogstatsd | {} \
                     ...`.",
                    self.text(next),
                    self.text(next)
                ),
            ));
        }
        Ok(Pipeline {
            head,
            stages,
            end,
            span: self.since(start),
        })
    }

    /// Returns an error if an operator follows a pipeline that has not ended, on the same line: the `|` before it is
    /// missing. Any other word, and a word on a line of its own, starts the next statement.
    fn missing_pipe(&self, pipeline: &Pipeline) -> Result<(), Error> {
        let next = self.peek();
        if pipeline.end.is_some() || next.kind != TokenKind::Ident || self.starts_line(0) {
            return Ok(());
        }
        if param_specs(self.text(next)).is_none() && !self.is_call(0) {
            return Ok(());
        }
        let last = match pipeline.stages.last() {
            None => match &pipeline.head {
                Head::Ref(reference) => reference.span,
                Head::Tuple(tuple) => tuple.span,
            },
            Some(Stage::Op(stage)) => stage.span,
            Some(Stage::Where(stage)) => stage.span,
        };
        let written = &self.source[last.range()];
        let written = if written.len() <= 40 && !written.contains('\n') {
            format!("`{}`", written)
        } else {
            match pipeline.stages.last() {
                Some(Stage::Op(stage)) => format!("`{}`", stage.op),
                Some(Stage::Where(_)) => "the `where`".to_string(),
                None => "the tuple".to_string(),
            }
        };
        Err(Error::new(
            next.span,
            format!("Expected `|` after {}, found `{}`.", written, self.text(next)),
        ))
    }

    /// Returns the error for a sink or fork that a `|` follows.
    #[cold]
    fn end_in_middle(&self, end: &End, in_branch: bool) -> Error {
        match end {
            End::Sink(sink) => {
                let word = sink_word(&sink.kind);
                let span = Span::new(sink.span.start, sink.span.start + word.len());
                let message = if in_branch {
                    format!(
                        "`{}` ends a branch; to send a copy and keep going, end the branch in a nested `fork` with a \
                         branch for each.",
                        word
                    )
                } else {
                    format!(
                        "`{}` ends a pipeline; to send a copy and keep going, name the stream with `let` and use it \
                         twice.",
                        word
                    )
                };
                Error::new(span, message)
            }
            End::Fork(fork) => Error::new(
                Span::new(fork.span.start, fork.span.start + "fork".len()),
                "`fork` ends a pipeline, since each of its branches ends in a sink or another `fork`; to continue \
                 after it, add the stages to its branches.",
            ),
        }
    }

    fn head(&mut self) -> Result<Head, Error> {
        let token = self.peek();
        match &token.kind {
            TokenKind::LParen => self.tuple().map(Head::Tuple),
            TokenKind::Ident => {
                let word = self.text(token);
                if !self.names_stream(0) && matches!(word, "emit" | "drop" | "exit" | "fork" | "where") {
                    return Err(Error::new(
                        token.span,
                        format!(
                            "Expected the start of a pipeline, found `{}`, which is a stage: it follows a stream and \
                             `|`, as in `dogstatsd | {} ...`.",
                            word, word
                        ),
                    ));
                }
                if word == "node" && self.peek_is(1, &TokenKind::Ident) && self.peek_is(2, &TokenKind::Eq) {
                    return Err(Error::new(
                        token.span,
                        "`node <id> = ...` is retired: write the operator as a stage of a pipeline, as in \
                         `dogstatsd | aggregate(window: 10s) as <id>`.",
                    ));
                }
                if word == "as" && !self.names_stream(0) {
                    // What follows a node's `as`, which ended a statement, without the node's name.
                    self.advance();
                    return Err(self.expected("a node name after `as`"));
                }
                if self.peek_is(1, &TokenKind::Dot) && self.peek_is(3, &TokenKind::Arrow) {
                    return Err(arrow(self.peek_at(3).span));
                }
                if word == "input" && self.peek_is(1, &TokenKind::Dot) {
                    return Err(Error::new(
                        token.span.to(self.peek_at(2).span),
                        "`input.` is retired: a pipeline names a scope input alone, as in `dogstatsd | ...`.",
                    ));
                }
                let reference = self.reference("the start of a pipeline: a stream's name, such as `dogstatsd`")?;
                if self.is_word(self.peek(), "as") && self.peek_is(1, &TokenKind::Ident) {
                    return Err(Error::new(
                        self.peek().span,
                        "`as` names a node, and a stream's name is not one: name a stream with `let <name> = ...`.",
                    ));
                }
                Ok(Head::Ref(reference))
            }
            _ => Err(self.expected(
                "a statement: `let <name> = <pipeline>`, a pipeline such as `dogstatsd | emit primary`, or `}` to \
                 close the policy",
            )),
        }
    }

    /// Parses `<name>` or `<name>.<port>`.
    fn reference(&mut self, what: &str) -> Result<Ref, Error> {
        let name = self.name(what)?;
        let mut port = None;
        if self.peek().kind == TokenKind::Dot {
            self.advance();
            port = Some(self.port_name("a port name after `.`")?);
        }
        Ok(Ref {
            span: self.since(name.span),
            name,
            port,
        })
    }

    fn tuple(&mut self) -> Result<Tuple, Error> {
        let open = self.advance().span;
        let mut items = Vec::new();
        loop {
            if self.peek().kind == TokenKind::RParen {
                if items.is_empty() {
                    return Err(Error::new(
                        open.to(self.peek().span),
                        "A tuple merges one or more streams: name them, as in `(a, b)`.",
                    ));
                }
                self.advance();
                break;
            }
            items.push(self.reference("a stream's name, such as `dogstatsd` or `s.true`")?);
            match self.separator(&TokenKind::RParen) {
                Some(true) => {}
                Some(false) => break,
                None => return Err(self.unclosed("`,` or `)` after the stream, to continue or close the tuple", open)),
            }
        }
        let id = self.as_clause()?;
        Ok(Tuple {
            items,
            id,
            span: self.since(open),
        })
    }

    /// Parses `as <id>`, if it comes next. An `as` with arguments right after it is an operator, which starts the next
    /// branch of a fork.
    ///
    /// An `as` that `|` or `.` follows names the stream the next statement starts from when it starts a line, as the
    /// printer writes statements; on the line of what it follows, it is an `as` missing its name.
    fn as_clause(&mut self) -> Result<Option<Name>, Error> {
        if !self.is_word(self.peek(), "as") || self.is_call(0) || (self.names_stream(0) && self.starts_line(0)) {
            return Ok(None);
        }
        self.advance();
        let name = self.name("a node name after `as`")?;
        if self.is_word(self.peek(), "as") && self.peek_is(1, &TokenKind::Ident) && !self.names_stream(0) {
            return Err(Error::new(
                self.peek().span,
                format!("A node takes one `as`, and this one already has `as {}`.", name.id),
            ));
        }
        Ok(Some(name))
    }

    /// Parses what follows a `|`: a stage, or a sink or fork that ends the pipeline. In a `let`, `name` is the
    /// stream's name, and an end is an error.
    fn item(&mut self, name: Option<&Name>, what: &str) -> Result<Item, Error> {
        let token = self.peek();
        if token.kind != TokenKind::Ident {
            return Err(self.expected(&format!(
                "{}: an operator such as `aggregate(window: 10s)`, `where <predicate>`, `fork {{ ... }}`, or a sink: \
                 `emit`, `drop`, or `exit`",
                what
            )));
        }
        // A word with arguments right after it is an operator, even a word that is otherwise a sink or `where`: the
        // core form of a node such as an `emit` whose parameters no sink writes, `emit(output: "o", to: [])`.
        if self.is_call(0) {
            return self.op_stage().map(|stage| Item::Stage(Stage::Op(stage)));
        }
        match self.text(token) {
            word @ ("emit" | "drop" | "exit" | "fork") => {
                if let Some(name) = name {
                    return Err(self.end_in_let(token, word, name));
                }
                if word == "fork" {
                    self.fork().map(|fork| Item::End(End::Fork(fork)))
                } else {
                    self.sink().map(|sink| Item::End(End::Sink(sink)))
                }
            }
            "where" => self.where_stage().map(|stage| Item::Stage(Stage::Where(stage))),
            _ => self.op_stage().map(|stage| Item::Stage(Stage::Op(stage))),
        }
    }

    /// Returns the error for a sink or fork in the pipeline of `let <name>`.
    #[cold]
    fn end_in_let(&self, token: &Token, word: &str, name: &Name) -> Error {
        Error::new(
            token.span,
            format!(
                "`{}` ends a pipeline, but `let {}` names a stream, which later statements continue: end it in a \
                 statement of its own, as in `{} | {} ...`, or write this pipeline without `let {} =`.",
                word, name.id, name.id, word, name.id
            ),
        )
    }

    fn where_stage(&mut self) -> Result<WhereStage, Error> {
        let keyword = self.advance().span;
        let start = self.peek().span;
        let predicate = self.predicate()?;
        let param_spans = vec![("predicate".to_string(), self.since(start))];
        let id = self.as_clause()?;
        Ok(WhereStage {
            predicate,
            param_spans,
            id,
            span: self.since(keyword),
        })
    }

    fn op_stage(&mut self) -> Result<OpStage, Error> {
        let (op, op_span) = self.ident("an operator name, such as `aggregate`", OperatorName::new)?;
        let mut params = Params::new();
        let mut param_spans = Vec::new();
        let mut positional = None;
        let next = self.peek();
        if next.kind == TokenKind::LParen {
            if next.span.start == op_span.end {
                match self.arguments(&op, &mut params, &mut param_spans) {
                    Ok(name) => positional = name,
                    Err(error) => {
                        if param_specs(op.as_str()).is_none() {
                            self.notes.push(unknown_operator(&op, op_span));
                        }
                        return Err(error);
                    }
                }
            } else if !self.tuple_follows() {
                return Err(Error::new(
                    next.span,
                    format!(
                        "Write the arguments of `{}` right after its name, without a space, as in `{}(...)`: a `(` \
                         after a space starts a tuple, the head of the next statement.",
                        op, op
                    ),
                ));
            }
        }
        let id = self.as_clause()?;
        Ok(OpStage {
            op,
            op_span,
            params,
            positional,
            param_spans,
            id,
            span: self.since(op_span),
        })
    }

    /// Returns `true` if the `(` that comes next opens what could be a tuple: names, perhaps with ports, separated by
    /// commas.
    fn tuple_follows(&self) -> bool {
        let mut offset = 1;
        loop {
            if !self.peek_is(offset, &TokenKind::Ident) {
                return false;
            }
            offset += 1;
            if self.peek_is(offset, &TokenKind::Dot) {
                if !matches!(self.peek_at(offset + 1).kind, TokenKind::Ident | TokenKind::Number(_)) {
                    return false;
                }
                offset += 2;
            }
            match self.peek_at(offset).kind {
                TokenKind::RParen => return true,
                TokenKind::Comma if self.peek_is(offset + 1, &TokenKind::RParen) => return true,
                TokenKind::Comma => offset += 1,
                _ => return false,
            }
        }
    }

    /// Parses an operator's parenthesized arguments, and returns the name of the parameter passed positionally, if
    /// one was.
    fn arguments(
        &mut self, op: &OperatorName, params: &mut Params, spans: &mut Vec<(String, Span)>,
    ) -> Result<Option<ParamName>, Error> {
        self.advance();
        let named_next = |parser: &Self| parser.peek().kind == TokenKind::Ident && parser.peek_is(1, &TokenKind::Colon);
        let specs = param_specs(op.as_str());

        let mut positional = None;
        if self.peek().kind != TokenKind::RParen && !named_next(self) {
            let Some(spec) = specs.and_then(|specs| specs.iter().find(|spec| spec.positional)) else {
                return Err(self.not_a_parameter(op, specs, true));
            };
            let name = ParamName::new(spec.name).map_err(|error| Error::new(self.peek().span, error.to_string()))?;
            let value = self.value(Some(&spec.ty), spec.name, spans)?;
            params.insert(name.clone(), value);
            positional = Some(name);
            match self.separator(&TokenKind::RParen) {
                Some(true) => {}
                Some(false) => return Ok(positional),
                None if matches!(literal_type(Some(&spec.ty)), Some(ParamType::Predicate)) => {
                    return Err(self.expected("`and`, `or`, `,`, or `)` after the predicate"))
                }
                None => return Err(self.expected("`,` or `)` after the parameter's value")),
            }
        }

        loop {
            if self.peek().kind == TokenKind::RParen {
                self.advance();
                return Ok(positional);
            }
            if !named_next(self) {
                return Err(self.not_a_parameter(op, specs, false));
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
            // Commas between named parameters may be left out.
            match self.peek().kind {
                TokenKind::Comma => {
                    self.advance();
                }
                TokenKind::RParen => {}
                _ if named_next(self) => {}
                _ if matches!(literal_type(ty), Some(ParamType::Predicate)) => {
                    return Err(self.expected("`and`, `or`, `,`, or `)` after the predicate"))
                }
                _ => return Err(self.expected("`,` or `)` after the parameter's value")),
            }
        }
    }

    #[cold]
    fn not_a_parameter(&self, op: &OperatorName, specs: Option<&[ParamSpec]>, first: bool) -> Error {
        let accepted = match specs {
            Some([]) => format!("; `{}` takes no parameters", op),
            Some(specs) => format!(
                "; `{}` takes: {}",
                op,
                specs.iter().map(|spec| spec.name).collect::<Vec<_>>().join(", ")
            ),
            None => String::new(),
        };
        let positional = if first && specs.is_some_and(|specs| !specs.is_empty()) {
            format!(" (`{}` has no parameter that may be passed without its name)", op)
        } else {
            String::new()
        };
        self.expected(&format!(
            "a parameter, written `<name>: <value>`{}, or `)`{}",
            positional, accepted
        ))
    }

    fn fork(&mut self) -> Result<Fork, Error> {
        let keyword = self.advance().span;
        let id = self.as_clause()?;
        let open = self
            .expect(
                TokenKind::LBrace,
                "`{` and the fork's branches, one per copy of the stream",
            )?
            .span;
        self.enter(open)?;
        let mut branches = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::RBrace => {
                    let close = self.advance().span;
                    if branches.is_empty() {
                        return Err(Error::new(
                            keyword.to(close),
                            "A fork has at least one branch, such as `emit primary`: each branch gets a copy of the \
                             stream.",
                        ));
                    }
                    break;
                }
                TokenKind::Eof => return Err(self.unclosed("a branch, or `}` to close the fork", open)),
                _ => branches.push(self.branch()?),
            }
        }
        self.leave();
        self.last_fork = Some(open);
        Ok(Fork {
            id,
            branches,
            span: self.since(keyword),
        })
    }

    /// Parses one branch of a fork: stages joined with `|`, ending in a sink or another fork.
    fn branch(&mut self) -> Result<Branch, Error> {
        let start = self.peek().span;
        if self.peek().kind == TokenKind::Pipe {
            return Err(Error::new(
                start,
                "A fork's branch starts with its first stage, without `|`, as in `where <predicate> | emit primary`.",
            ));
        }
        let mut stages = Vec::new();
        loop {
            match self.item(None, "a branch's stage, or `}` to close the fork")? {
                Item::Stage(stage) => {
                    stages.push(stage);
                    if self.peek().kind != TokenKind::Pipe {
                        return Err(self.expected(
                            "`|` and the branch's next stage: each branch of a fork ends in a sink (`emit`, `drop`, \
                             or `exit`) or another `fork`",
                        ));
                    }
                    self.advance();
                }
                Item::End(end) => {
                    if self.peek().kind == TokenKind::Pipe {
                        return Err(self.end_in_middle(&end, true));
                    }
                    return Ok(Branch {
                        stages,
                        end,
                        span: self.since(start),
                    });
                }
            }
        }
    }

    fn sink(&mut self) -> Result<Sink, Error> {
        let keyword = self.advance();
        let kind = match self.text(keyword) {
            "emit" => self.emit()?,
            "drop" => SinkKind::Drop,
            _ => {
                let (name, span) = self.ident("a scope exit name after `exit`", ExitName::new)?;
                SinkKind::Exit { name, span }
            }
        };
        let id = self.as_clause()?;
        Ok(Sink {
            kind,
            id,
            span: self.since(keyword.span),
        })
    }

    /// Parses what follows `emit`.
    fn emit(&mut self) -> Result<SinkKind, Error> {
        let token = self.peek();
        match &token.kind {
            TokenKind::Ident if self.text(token) == "output" => {
                self.advance();
                let (output, span) = self.ident("an output ID after `emit output`", OutputId::new)?;
                Ok(SinkKind::Output { output, span })
            }
            TokenKind::LBracket => {
                let (names, span) = self.recipient_names()?;
                Ok(SinkKind::Recipients { names, span })
            }
            TokenKind::Str(name) => {
                self.advance();
                Ok(SinkKind::Recipients {
                    names: vec![name.clone()],
                    span: token.span,
                })
            }
            TokenKind::Ident if self.text(token) != "as" => {
                self.advance();
                Ok(SinkKind::Recipients {
                    names: vec![self.text(token).to_string()],
                    span: token.span,
                })
            }
            _ => Err(self.expected(
                "what `emit` sends to: a recipient such as `primary`, a list such as `[primary, \
                 \"https://intake.example.com\"]`, or `output <output>`",
            )),
        }
    }

    /// Parses a bracketed list of recipient names, each an identifier or a quoted string, and returns them with the
    /// list's span.
    fn recipient_names(&mut self) -> Result<(Vec<String>, Span), Error> {
        let open = self.advance().span;
        let mut names = Vec::new();
        loop {
            let token = self.peek();
            match &token.kind {
                TokenKind::RBracket => {
                    self.advance();
                    return Ok((names, open.to(token.span)));
                }
                TokenKind::Str(value) => {
                    self.advance();
                    names.push(value.clone());
                }
                TokenKind::Ident => {
                    self.advance();
                    names.push(self.text(token).to_string());
                }
                _ => {
                    return Err(self.unclosed(
                        "a recipient name (an identifier, or a quoted string such as a URL), or `]` to close the list",
                        open,
                    ))
                }
            }
            match self.separator(&TokenKind::RBracket) {
                Some(true) => {}
                Some(false) => return Ok((names, self.since(open))),
                None => {
                    return Err(self.unclosed("`,` or `]` after the recipient, to continue or close the list", open))
                }
            }
        }
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
                    "config" if self.peek_is(1, &TokenKind::Dot) => {
                        let binding = self.binding()?;
                        return Ok(single_member(BINDING_MEMBER, Value::String(binding.to_string())));
                    }
                    "config" if self.peek_is(1, &TokenKind::LParen) => return Err(self.retired_config()),
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
                     a record `{...}`, `config.<binding>`, or `blob(\"<hash>\")`",
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

    /// Parses `config.<segment>.<segment>...`, a configuration binding.
    fn binding(&mut self) -> Result<BindingName, Error> {
        let start = self.advance().span;
        let mut name = String::new();
        while self.peek().kind == TokenKind::Dot {
            self.advance();
            let token = self.peek();
            // The lexer makes a segment right after a `.` a word, even one that starts with a digit, unless it is a
            // plain integer.
            let segment = self.text(token);
            if !matches!(token.kind, TokenKind::Ident | TokenKind::Number(_)) || !is_word_text(segment) {
                return Err(self.expected("the next segment of the binding's name after `.`, as in `config.host.tags`"));
            }
            self.advance();
            if !name.is_empty() {
                name.push('.');
            }
            name.push_str(segment);
        }
        let span = self.since(start);
        BindingName::new(name).map_err(|error| Error::new(span, error.to_string()))
    }

    /// Returns the error for `config("...")`, which the next tokens start.
    #[cold]
    fn retired_config(&self) -> Error {
        let start = self.peek().span;
        let (span, example) = match (&self.peek_at(2).kind, &self.peek_at(3).kind) {
            (TokenKind::Str(name), TokenKind::RParen) => (start.to(self.peek_at(3).span), format!("config.{}", name)),
            _ => (start, "config.host.tags".to_string()),
        };
        Error::new(
            span,
            format!(
                "`config(\"...\")` is retired: a configuration binding is written `config.` and the binding's name, \
                 as in `{}`.",
                example
            ),
        )
    }

    // Predicates.

    /// Parses a predicate: `or` of `and` of possibly negated primaries.
    ///
    /// An `and` or `or` that `|` or `.` follows is not a junction but the name of the stream the next statement starts
    /// from, as after a `where` that ends a statement.
    fn predicate(&mut self) -> Result<Value, Error> {
        self.enter(self.peek().span)?;
        let mut operands = vec![self.conjunction()?];
        while self.is_word(self.peek(), "or") && !self.names_stream(0) {
            self.advance();
            operands.push(self.conjunction()?);
        }
        self.leave();
        Ok(junction("any", operands))
    }

    fn conjunction(&mut self) -> Result<Value, Error> {
        let mut operands = vec![self.negation()?];
        while self.is_word(self.peek(), "and") && !self.names_stream(0) {
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
            // A value written raw where a predicate is expected, so that any artifact prints and parses back.
            TokenKind::LBrace | TokenKind::LBracket | TokenKind::Str(_) | TokenKind::Number(_) => self.raw(),
            TokenKind::Ident => match self.text(token) {
                "null" => self.raw(),
                "blob" if self.peek_is(1, &TokenKind::LParen) => self.raw(),
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
        let word = self.text(token);
        let leaf = match word {
            "true" | "false" => {
                self.advance();
                Predicate::Const(word == "true")
            }
            "name" => {
                self.advance();
                let (value, negated) = self.string_match()?;
                negate(Predicate::Name(value), negated)
            }
            "intake_name" => {
                self.advance();
                let (value, negated) = self.string_match()?;
                negate(Predicate::IntakeName(value), negated)
            }
            "host" if self.is_word(self.peek_at(1), "is") => {
                self.advance();
                self.advance();
                let set = self.presence()?;
                negate(Predicate::HostUnset {}, set)
            }
            "host" => {
                self.advance();
                let (value, negated) = self.string_match()?;
                negate(Predicate::Host(value), negated)
            }
            "tags" | "origin_tags" if matches!(self.peek_at(1).kind, TokenKind::Dot | TokenKind::LBracket) => {
                self.advance();
                let key = self.tag_key(word)?;
                let origin = word == "origin_tags";
                if self.is_word(self.peek(), "is") {
                    self.advance();
                    let set = self.presence()?;
                    let leaf = if origin {
                        Predicate::HasOriginTag(key)
                    } else {
                        Predicate::HasTag(key)
                    };
                    negate(leaf, !set)
                } else {
                    if self.peek().kind == TokenKind::Dot {
                        return Err(self.expected(&format!(
                            "`is set`, `is unset`, or a string match after the tag key: a key that is not an \
                             identifier, such as one with a `.` in it, is quoted, as in \
                             `{}[\"app.kubernetes.io/name\"]`",
                            word
                        )));
                    }
                    let next = self.peek();
                    let matches_next = matches!(next.kind, TokenKind::EqEq | TokenKind::NotEq)
                        || (next.kind == TokenKind::Ident
                            && matches!(self.text(next), "starts_with" | "in" | "prefix_in" | "matches"));
                    if !matches_next {
                        return Err(self.expected(&format!(
                            "`is set`, `is unset`, or a string match after `{}.<key>`: `== \"<s>\"`, `!= \"<s>\"`, \
                             `starts_with \"<s>\"`, `starts_with [...]`, `in [...]`, `in config.<binding>`, or \
                             `matches(values: ..., prefix: ...)`",
                            word
                        )));
                    }
                    let (value, negated) = self.string_match()?;
                    let tag = TagMatch { key, value };
                    let leaf = if origin {
                        Predicate::OriginTag(tag)
                    } else {
                        Predicate::Tag(tag)
                    };
                    negate(leaf, negated)
                }
            }
            "kind" => {
                self.advance();
                if !self.is_word(self.peek(), "in") {
                    return Err(self.expected("`in` and a list of kinds after `kind`"));
                }
                self.advance();
                Predicate::KindIn(self.kinds()?)
            }
            "config" if self.peek_is(1, &TokenKind::Dot) => Predicate::Flag(Operand::config(self.binding()?)),
            "config" if call => return Err(self.retired_config()),
            "is_empty" if call => {
                self.advance();
                self.advance();
                let list = self.strings_operand()?;
                self.expect(TokenKind::RParen, "`)` after the list")?;
                Predicate::IsEmpty(list)
            }
            "all" | "any" | "has_tag" | "has_origin_tag" | "tag" | "origin_tag" | "flag" if call => {
                return Err(self.retired_call(token))
            }
            "is_empty" => {
                self.advance();
                return Err(self.expected("`(` after `is_empty`, as in `is_empty(config.x)` or `is_empty([])`"));
            }
            _ => return Err(self.expected(PREDICATE_EXPECTED)),
        };
        serde_json::to_value(&leaf).map_err(|error| Error::new(token.span, format!("Invalid predicate: {}", error)))
    }

    /// Returns the error for a retired predicate written as a call, which `token` names.
    #[cold]
    fn retired_call(&self, token: &Token) -> Error {
        let word = self.text(token);
        // The tag key, written as the new syntax writes it, when the call gives one as a string.
        let key = |field: &str| match (&self.peek_at(2).kind, &self.peek_at(3).kind) {
            (TokenKind::Str(key), TokenKind::RParen) if is_identifier(key) => format!("{}.{}", field, key),
            (TokenKind::Str(_), TokenKind::RParen) => format!("{}[{}]", field, self.text(self.peek_at(2))),
            _ => format!("{}.<key>", field),
        };
        let message = match word {
            "all" => "`all(...)` is retired: join the predicates with `and`, as in `a and b and c`.".to_string(),
            "any" => "`any(...)` is retired: join the predicates with `or`, as in `a or b or c`.".to_string(),
            "has_tag" => format!("`has_tag(...)` is retired: write `{} is set`.", key("tags")),
            "has_origin_tag" => format!(
                "`has_origin_tag(...)` is retired: write `{} is set`.",
                key("origin_tags")
            ),
            "tag" => format!(
                "`tag(...)` is retired: write `{}` and the match, as in `{} == \"platform\"`.",
                key("tags"),
                key("tags")
            ),
            "origin_tag" => format!(
                "`origin_tag(...)` is retired: write `{}` and the match, as in `{} starts_with \"web-\"`.",
                key("origin_tags"),
                key("origin_tags")
            ),
            _ => "`flag(...)` is retired: write the binding alone, as in `config.multi_region_failover.enabled`, or \
                  the constant `true` or `false`."
                .to_string(),
        };
        Error::new(token.span, message)
    }

    /// Parses `set` or `unset` after `is`, and returns `true` for `set`.
    fn presence(&mut self) -> Result<bool, Error> {
        let token = self.peek();
        let set = if self.is_word(token, "set") {
            true
        } else if self.is_word(token, "unset") {
            false
        } else {
            return Err(self.expected("`set` or `unset` after `is`"));
        };
        self.advance();
        Ok(set)
    }

    /// Parses `.<key>` or `["<key>"]` after `tags` or `origin_tags`, which is `field`.
    fn tag_key(&mut self, field: &str) -> Result<String, Error> {
        if self.advance().kind == TokenKind::Dot {
            let token = self.peek();
            if token.kind != TokenKind::Ident {
                return Err(self.expected(&format!(
                    "a tag key after `{}.`: a key that is not an identifier is quoted, as in \
                     `{}[\"app.kubernetes.io/name\"]`",
                    field, field
                )));
            }
            self.advance();
            return Ok(self.text(token).to_string());
        }
        let open = self.tokens[self.pos - 1].span;
        let (key, _) = self.string(&format!("the tag key as a string after `{}[`", field))?;
        if self.peek().kind != TokenKind::RBracket {
            return Err(self.unclosed("`]` to close the tag key", open));
        }
        self.advance();
        Ok(key)
    }

    /// Parses a string match, and returns it with whether it was written `!=`, which negates it.
    fn string_match(&mut self) -> Result<(StringMatch, bool), Error> {
        let token = self.peek();
        let word = if token.kind == TokenKind::Ident {
            self.text(token)
        } else {
            ""
        };
        let config_next =
            |parser: &Self, kind: TokenKind| parser.is_word(parser.peek(), "config") && parser.peek_is(1, &kind);
        match &token.kind {
            TokenKind::EqEq => {
                self.advance();
                return Ok((StringMatch::Eq(self.string("a string after `==`")?.0), false));
            }
            TokenKind::NotEq => {
                self.advance();
                return Ok((StringMatch::Eq(self.string("a string after `!=`")?.0), true));
            }
            _ => {}
        }
        let value = match word {
            "starts_with" => {
                self.advance();
                match self.peek().kind {
                    TokenKind::Str(_) => StringMatch::Prefix(self.string("a string")?.0),
                    TokenKind::LBracket => StringMatch::PrefixIn(self.strings()?),
                    _ if config_next(self, TokenKind::Dot) => StringMatch::Matches(SetMatch {
                        values: Operand::config(self.binding()?),
                        prefix: Operand::literal(true),
                    }),
                    _ if config_next(self, TokenKind::LParen) => return Err(self.retired_config()),
                    _ => {
                        return Err(
                            self.expected("a string, a list of strings, or `config.<binding>` after `starts_with`")
                        )
                    }
                }
            }
            "in" => {
                self.advance();
                if config_next(self, TokenKind::LParen) {
                    return Err(self.retired_config());
                }
                if config_next(self, TokenKind::Dot) {
                    StringMatch::Matches(SetMatch {
                        values: Operand::config(self.binding()?),
                        prefix: Operand::literal(false),
                    })
                } else if self.peek().kind == TokenKind::LBracket {
                    StringMatch::In(self.strings()?)
                } else {
                    return Err(
                        self.expected("a list of strings, such as `[\"a\", \"b\"]`, or `config.<binding>` after `in`")
                    );
                }
            }
            "prefix_in" => {
                return Err(Error::new(
                    token.span,
                    "`prefix_in` is retired: write `starts_with` and the list, as in `name starts_with [\"a.\", \
                     \"b.\"]`.",
                ))
            }
            "matches" if self.peek_is(1, &TokenKind::LParen) => {
                self.advance();
                StringMatch::Matches(self.set_match()?)
            }
            "matches" => {
                self.advance();
                return Err(self.expected("`(` after `matches`, as in `matches(values: [\"a\"], prefix: false)`"));
            }
            _ => {
                return Err(self.expected(
                    "a string match: `== \"<s>\"`, `!= \"<s>\"`, `starts_with \"<s>\"`, `starts_with [...]`, \
                     `in [...]`, `in config.<binding>`, or `matches(values: ..., prefix: ...)`",
                ))
            }
        };
        Ok((value, false))
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
        if self.is_word(self.peek(), "config") && self.peek_is(1, &TokenKind::Dot) {
            return self.binding().map(Operand::config);
        }
        if self.is_word(self.peek(), "config") && self.peek_is(1, &TokenKind::LParen) {
            return Err(self.retired_config());
        }
        if self.peek().kind != TokenKind::LBracket {
            return Err(self.expected("a list of strings, or `config.<binding>`"));
        }
        self.strings().map(Operand::literal)
    }

    fn bool_operand(&mut self) -> Result<Operand<bool>, Error> {
        let token = self.peek();
        if self.is_word(token, "config") && self.peek_is(1, &TokenKind::Dot) {
            return self.binding().map(Operand::config);
        }
        if self.is_word(token, "config") && self.peek_is(1, &TokenKind::LParen) {
            return Err(self.retired_config());
        }
        if self.is_word(token, "true") || self.is_word(token, "false") {
            self.advance();
            return Ok(Operand::literal(self.text(token) == "true"));
        }
        Err(self.expected("`true`, `false`, or `config.<binding>`"))
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
    "a predicate, such as `name == \"requests\"`, `tags.env is set`, `kind in [counter]`, \
                                  `true`, `not <predicate>`, or `(<predicate>)`";

/// Returns the keyword a sink starts with.
fn sink_word(kind: &SinkKind) -> &'static str {
    match kind {
        SinkKind::Recipients { .. } | SinkKind::Output { .. } => "emit",
        SinkKind::Drop => "drop",
        SinkKind::Exit { .. } => "exit",
    }
}

/// Returns `true` if `text` is an identifier token, so a tag key can be written after `tags.` without quotes.
pub(crate) fn is_identifier(text: &str) -> bool {
    let mut bytes = text.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Returns `true` if `text` is made only of identifier characters.
fn is_word_text(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Returns `predicate`, negated if `negated` is `true`.
fn negate(predicate: Predicate, negated: bool) -> Predicate {
    if negated {
        Predicate::Not(Box::new(predicate))
    } else {
        predicate
    }
}

#[cold]
fn arrow(span: Span) -> Error {
    Error::new(
        span,
        "`->` is retired: join a pipeline's stages with `|`, as in `dogstatsd | emit primary`.",
    )
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
            "This nests more than {} levels deep, the most a policy may. Flatten the value, predicate, or fork.",
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
fn literal_type(ty: Option<&'static ParamType>) -> Option<&'static ParamType> {
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

#[cfg(test)]
mod tests;
