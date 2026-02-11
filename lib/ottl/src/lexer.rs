use logos::Logos;

/// OTTL language tokens
#[derive(Logos, Debug, PartialEq, Eq, Clone, Hash)]
#[logos(skip r"[ \t]+")] // Skip spaces and tabs
pub enum Token<'a> {
    // ===== Keywords =====
    #[token("where")]
    Where,

    #[token("or")]
    Or,

    #[token("and")]
    And,

    #[token("not")]
    Not,

    #[token("true")]
    True,

    #[token("false")]
    False,

    #[token("nil")]
    Nil,

    // ===== Comparison operators =====
    #[token("==")]
    Eq,

    #[token("!=")]
    NotEq,

    #[token("<=")]
    LessEq,

    #[token(">=")]
    GreaterEq,

    #[token("<")]
    Less,

    #[token(">")]
    Greater,

    // ===== Arithmetic operators =====
    #[token("+")]
    Plus,

    #[token("-")]
    Minus,

    #[token("*")]
    Multiply,

    #[token("/")]
    Divide,

    // ===== Delimiters =====
    #[token("(")]
    LParen,

    #[token(")")]
    RParen,

    #[token("[")]
    LBracket,

    #[token("]")]
    RBracket,

    #[token("{")]
    LBrace,

    #[token("}")]
    RBrace,

    #[token(",")]
    Comma,

    #[token(".")]
    Dot,

    #[token(":")]
    Colon,

    #[token("=")]
    Assign,

    // ===== Literals =====
    /// String literal: "..."
    #[regex(r#""[^"\\]*(?:\\.[^"\\]*)*""#, |lex| lex.slice())]
    StringLiteral(&'a str),

    /// Bytes literal: 0xC0FFEE
    #[regex(r"0x[0-9a-fA-F]+", |lex| lex.slice())]
    BytesLiteral(&'a str),

    /// Float literal: 6.14, .5
    #[regex(r"[0-9]+\.[0-9]*|\.[0-9]+", |lex| lex.slice())]
    FloatLiteral(&'a str),

    /// Integer literal: 42, 10, 5
    #[regex(r"[0-9]+", priority = 2, callback = |lex| lex.slice())]
    IntLiteral(&'a str),

    // ===== Identifiers =====
    /// Uppercase identifier (Converter or Enum)
    #[regex(r"[A-Z][a-zA-Z0-9_]*", |lex| lex.slice())]
    UpperIdent(&'a str),

    /// Lowercase identifier (Editor, path, named arg)
    #[regex(r"[a-z][a-zA-Z0-9_]*", priority = 1, callback = |lex| lex.slice())]
    LowerIdent(&'a str),
}

/// Lexer error with position information
#[derive(Debug, Clone)]
pub struct LexerError {
    /// Position in the input where the error occurred
    pub position: usize,
    /// The invalid character or slice that caused the error
    pub invalid_slice: String,
}

impl std::fmt::Display for LexerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Invalid token '{}' at position {}",
            self.invalid_slice, self.position
        )
    }
}

/// Lexical analysis result
pub struct Lexer<'a> {
    lexer: logos::Lexer<'a, Token<'a>>,
}

impl<'a> Lexer<'a> {
    /// Collect tokens with their positions (spans)
    /// Returns an error if any invalid token is encountered
    pub fn collect_with_spans(input: &'a str) -> Result<Vec<(Token<'a>, std::ops::Range<usize>)>, LexerError> {
        let mut lexer = Token::lexer(input);
        let mut tokens = Vec::new();
        while let Some(result) = lexer.next() {
            match result {
                Ok(token) => tokens.push((token, lexer.span())),
                Err(_) => {
                    let span = lexer.span();
                    let invalid_slice = input[span.clone()].to_string();
                    return Err(LexerError {
                        position: span.start,
                        invalid_slice,
                    });
                }
            }
        }
        Ok(tokens)
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<Token<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        self.lexer.next()
    }
}
