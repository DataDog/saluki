use logos::Logos;

/// OTTL language tokens
#[derive(Logos, Debug, PartialEq, Clone)]
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
    Star,

    #[token("/")]
    Slash,

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

    /// Float literal: 3.14, .5, -2.0
    #[regex(r"[+-]?([0-9]+\.[0-9]*|\.[0-9]+)", |lex| lex.slice())]
    FloatLiteral(&'a str),

    /// Integer literal: 42, -10, +5
    #[regex(r"[+-]?[0-9]+", priority = 2, callback = |lex| lex.slice())]
    IntLiteral(&'a str),

    // ===== Identifiers =====
    /// Uppercase identifier (Converter or Enum)
    #[regex(r"[A-Z][a-zA-Z0-9_]*", |lex| lex.slice())]
    UpperIdent(&'a str),

    /// Lowercase identifier (Editor, path, named arg)
    #[regex(r"[a-z][a-zA-Z0-9_]*", priority = 1, callback = |lex| lex.slice())]
    LowerIdent(&'a str),
}

/// Lexical analysis result
pub struct Lexer<'a> {
    _lexer: logos::Lexer<'a, Token<'a>>,
}

impl<'a> Lexer<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            _lexer: Token::lexer(input),
        }
    }

    /// Collect all tokens into a vector
    pub fn collect_tokens(input: &'a str) -> Vec<Token<'a>> {
        Token::lexer(input)
            .filter_map(|result| result.ok())
            .collect()
    }

    /// Collect tokens with their positions (spans)
    pub fn collect_with_spans(input: &'a str) -> Vec<(Token<'a>, std::ops::Range<usize>)> {
        let mut lexer = Token::lexer(input);
        let mut tokens = Vec::new();
        while let Some(result) = lexer.next() {
            if let Ok(token) = result {
                tokens.push((token, lexer.span()));
            }
        }
        tokens
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Result<Token<'a>, ()>;

    fn next(&mut self) -> Option<Self::Item> {
        self._lexer.next()
    }
}
