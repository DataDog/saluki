//! Chumsky parser for OTTL

use chumsky::prelude::*;

use super::ast::*;
use crate::lexer::Token;
use crate::{CallbackMap, EnumMap, Value};

/// Type alias for our input - a slice of tokens
pub type TokenInput<'src> = &'src [Token<'src>];

/// Type alias for parser extra
pub type ParserExtra<'src> = extra::Err<Rich<'src, Token<'src>>>;

/// Unescape a quoted string literal (removes quotes and handles escapes)
#[inline]
fn unescape(s: &str) -> String {
    s[1..s.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
}

// =====================================================================================================================
// Parser Components
// =====================================================================================================================

/// Parser for literal values.
fn literal_parser<'a>() -> impl chumsky::Parser<'a, TokenInput<'a>, ValueExpr, ParserExtra<'a>> + Clone {
    let string_literal = select_ref! {
        Token::StringLiteral(s) => Value::string(unescape(s))
    };

    let int_literal = just(&Token::Minus)
        .or_not()
        .then(select_ref! {
            Token::IntLiteral(s) => s.parse::<i64>().unwrap_or(0)
        })
        .map(|(neg, val)| {
            let val = if neg.is_some() { -val } else { val };
            Value::Int(val)
        });

    let float_literal = just(&Token::Minus)
        .or_not()
        .then(select_ref! {
            Token::FloatLiteral(s) => s.parse::<f64>().unwrap_or(0.0)
        })
        .map(|(neg, val)| {
            let val = if neg.is_some() { -val } else { val };
            Value::Float(val)
        });

    let bytes_literal = select_ref! {
        Token::BytesLiteral(s) => {
            let hex = &s[2..];
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| {
                    let end = (i + 2).min(hex.len());
                    u8::from_str_radix(&hex[i..end], 16).unwrap_or(0)
                })
                .collect();
            Value::bytes(bytes)
        }
    };

    let bool_literal = select_ref! {
        Token::True => Value::Bool(true),
        Token::False => Value::Bool(false),
    };

    let nil_literal = just(&Token::Nil).to(Value::Nil);

    choice((
        float_literal,
        int_literal,
        string_literal,
        bytes_literal,
        bool_literal,
        nil_literal,
    ))
    .map(ValueExpr::Literal)
}

/// Parser for index expressions: "[" (string | int) "]"
fn index_parser<'a>() -> impl chumsky::Parser<'a, TokenInput<'a>, IndexExpr, ParserExtra<'a>> + Clone {
    choice((
        select_ref! { Token::StringLiteral(s) => IndexExpr::String(unescape(s)) },
        select_ref! { Token::IntLiteral(s) => IndexExpr::Int(s.parse::<usize>().unwrap_or(0)) },
    ))
    .delimited_by(just(&Token::LBracket), just(&Token::RBracket))
}

/// Parser for identifiers (lower or upper case)
fn ident_parser<'a>(upper: bool) -> impl chumsky::Parser<'a, TokenInput<'a>, String, ParserExtra<'a>> + Clone {
    if upper {
        select_ref! { Token::UpperIdent(s) => s.to_string() }.boxed()
    } else {
        select_ref! { Token::LowerIdent(s) => s.to_string() }.boxed()
    }
}

/// Parser for path expressions: lower_ident ("." ident_segment)* index*
fn path_parser<'a>() -> impl chumsky::Parser<'a, TokenInput<'a>, PathExpr, ParserExtra<'a>> + Clone {
    let lower_ident = ident_parser(false);
    let ident_segment = ident_parser(false).or(ident_parser(true));
    let index = index_parser();

    lower_ident
        .then(
            just(&Token::Dot)
                .ignore_then(ident_segment)
                .repeated()
                .collect::<Vec<_>>(),
        )
        .then(index.repeated().collect::<Vec<_>>())
        .map(|((first, rest), indexes)| {
            let mut segments = vec![first];
            segments.extend(rest);
            PathExpr { segments, indexes }
        })
}

/// Parser for comparison operators
fn comp_op_parser<'a>() -> impl chumsky::Parser<'a, TokenInput<'a>, CompOp, ParserExtra<'a>> + Clone {
    choice((
        just(&Token::Eq).to(CompOp::Eq),
        just(&Token::NotEq).to(CompOp::NotEq),
        just(&Token::LessEq).to(CompOp::LessEq),
        just(&Token::GreaterEq).to(CompOp::GreaterEq),
        just(&Token::Less).to(CompOp::Less),
        just(&Token::Greater).to(CompOp::Greater),
    ))
}

/// Parser for argument list
fn arg_list_parser<'a>(
    arg_value: impl chumsky::Parser<'a, TokenInput<'a>, ValueExpr, ParserExtra<'a>> + Clone + 'a,
) -> impl chumsky::Parser<'a, TokenInput<'a>, Vec<ArgExpr>, ParserExtra<'a>> + Clone + 'a {
    let named_arg = ident_parser(false)
        .then_ignore(just(&Token::Assign))
        .then(arg_value.clone())
        .map(|(name, value)| ArgExpr::Named { name, value });

    let positional_arg = arg_value.map(ArgExpr::Positional);

    named_arg
        .or(positional_arg)
        .separated_by(just(&Token::Comma))
        .allow_trailing()
        .collect::<Vec<_>>()
}

/// Wraps a MathExpr into ValueExpr, unwrapping simple Primary values
fn math_to_value_expr(math: MathExpr) -> ValueExpr {
    match math {
        MathExpr::Primary(v) => v,
        other => ValueExpr::Math(Box::new(other)),
    }
}

/// Creates a math expression parser
fn make_math_expr<'a>(
    value_expr: impl chumsky::Parser<'a, TokenInput<'a>, ValueExpr, ParserExtra<'a>> + Clone + 'a,
) -> impl chumsky::Parser<'a, TokenInput<'a>, MathExpr, ParserExtra<'a>> + Clone + 'a {
    recursive(move |math_expr| {
        let math_literal = choice((
            select_ref! {
                Token::FloatLiteral(s) => {
                    let val: f64 = s.parse().unwrap_or(0.0);
                    MathExpr::Primary(ValueExpr::Literal(Value::Float(val)))
                }
            },
            select_ref! {
                Token::IntLiteral(s) => {
                    let val: i64 = s.parse().unwrap_or(0);
                    MathExpr::Primary(ValueExpr::Literal(Value::Int(val)))
                }
            },
        ));

        let paren_math = math_expr
            .clone()
            .delimited_by(just(&Token::LParen), just(&Token::RParen));

        let math_value = value_expr.clone().try_map(|v, span| match &v {
            ValueExpr::FunctionCall(_) => Ok(MathExpr::Primary(v)),
            ValueExpr::Path(_) => Ok(MathExpr::Primary(v)),
            ValueExpr::Literal(_) => Ok(MathExpr::Primary(v)),
            ValueExpr::List(_) => Ok(MathExpr::Primary(v)),
            ValueExpr::Map(_) => Ok(MathExpr::Primary(v)),
            _ => Err(Rich::custom(span, "Expected converter, path, literal, list, or map")),
        });

        let primary = choice((paren_math, math_literal, math_value));

        let unary_op = choice((just(&Token::Plus).to(true), just(&Token::Minus).to(false)));

        let factor = unary_op.or_not().then(primary).map(|(op, expr)| match op {
            Some(false) => MathExpr::Negate(Box::new(expr)),
            _ => expr,
        });

        let mul_op = choice((
            just(&Token::Multiply).to(MathOp::Mul),
            just(&Token::Divide).to(MathOp::Div),
        ));

        let term = factor
            .clone()
            .foldl(mul_op.then(factor).repeated(), |left, (op, right)| MathExpr::Binary {
                left: Box::new(left),
                op,
                right: Box::new(right),
            });

        let add_op = choice((just(&Token::Plus).to(MathOp::Add), just(&Token::Minus).to(MathOp::Sub)));

        term.clone()
            .foldl(add_op.then(term).repeated(), |left, (op, right)| MathExpr::Binary {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
    })
}

// =====================================================================================================================
// Main Parser
// =====================================================================================================================

/// Build the chumsky parser for OTTL.
pub fn build_parser<'a>(
    editors_map: &'a CallbackMap, converters_map: &'a CallbackMap, enums_map: &'a EnumMap,
) -> impl chumsky::Parser<'a, TokenInput<'a>, RootExpr, ParserExtra<'a>> + 'a {
    let literal = literal_parser();
    let index = index_parser();
    let path = path_parser();
    let comp_op = comp_op_parser();

    let enum_parser = {
        ident_parser(true).try_map(move |name, span| {
            if let Some(&val) = enums_map.get(&name) {
                Ok(ValueExpr::Literal(Value::Int(val)))
            } else {
                Err(Rich::custom(span, format!("Unknown enum: {}", name)))
            }
        })
    };

    let path_for_bool = path.clone();

    let value_expr = recursive(|value_expr| {
        let list = value_expr
            .clone()
            .separated_by(just(&Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(&Token::LBracket), just(&Token::RBracket))
            .map(ValueExpr::List);

        let map_entry = select_ref! { Token::StringLiteral(s) => unescape(s) }
            .then_ignore(just(&Token::Colon))
            .then(value_expr.clone());

        let map = map_entry
            .separated_by(just(&Token::Comma))
            .allow_trailing()
            .collect::<Vec<_>>()
            .delimited_by(just(&Token::LBrace), just(&Token::RBrace))
            .map(ValueExpr::Map);

        let arg_value = make_math_expr(value_expr.clone()).map(math_to_value_expr);
        let arg_list = arg_list_parser(arg_value);

        let converter_call = ident_parser(true)
            .then(arg_list.delimited_by(just(&Token::LParen), just(&Token::RParen)))
            .then(index.clone().repeated().collect::<Vec<_>>())
            .map(move |((name, args), indexes)| {
                let callback = converters_map.get(&name).cloned();
                ValueExpr::FunctionCall(Box::new(FunctionCall {
                    name,
                    is_editor: false,
                    args,
                    indexes,
                    callback,
                }))
            });

        choice((
            converter_call,
            list,
            map,
            enum_parser.clone(),
            path.clone().map(ValueExpr::Path),
            literal.clone(),
        ))
    });

    let editor_call = {
        let arg_value = make_math_expr(value_expr.clone()).map(math_to_value_expr);
        let arg_list = arg_list_parser(arg_value);

        ident_parser(false)
            .then(arg_list.delimited_by(just(&Token::LParen), just(&Token::RParen)))
            .map(move |(name, args)| FunctionCall {
                callback: editors_map.get(&name).cloned(),
                name,
                is_editor: true,
                args,
                indexes: Vec::new(),
            })
    };

    let math_expr_for_comparison = make_math_expr(value_expr.clone());
    let comparison_value = math_expr_for_comparison.clone().map(|m| ValueExpr::Math(Box::new(m)));

    let bool_expr = recursive({
        let value_expr = value_expr.clone();
        let comparison_value = comparison_value.clone();
        let path = path_for_bool.clone();
        move |bool_expr| {
            let bool_literal_expr = select_ref! {
                Token::True => BoolExpr::Literal(true),
                Token::False => BoolExpr::Literal(false),
            };

            let comparison = comparison_value
                .clone()
                .then(comp_op.clone())
                .then(comparison_value.clone())
                .map(|((left, op), right)| BoolExpr::Comparison { left, op, right });

            let bool_converter = value_expr.clone().try_map(|v, span| {
                if let ValueExpr::FunctionCall(fc) = v {
                    Ok(BoolExpr::Converter(fc))
                } else {
                    Err(Rich::custom(span, "Expected converter call"))
                }
            });

            let bool_path = path.clone().map(BoolExpr::Path);

            let bool_primary = choice((
                bool_expr
                    .clone()
                    .delimited_by(just(&Token::LParen), just(&Token::RParen)),
                comparison,
                bool_literal_expr,
                bool_converter,
                bool_path,
            ));

            let bool_factor = just(&Token::Not).or_not().then(bool_primary).map(|(not, expr)| {
                if not.is_some() {
                    BoolExpr::Not(Box::new(expr))
                } else {
                    expr
                }
            });

            let bool_term = bool_factor
                .clone()
                .foldl(just(&Token::And).ignore_then(bool_factor).repeated(), |left, right| {
                    BoolExpr::And(Box::new(left), Box::new(right))
                });

            bool_term
                .clone()
                .foldl(just(&Token::Or).ignore_then(bool_term).repeated(), |left, right| {
                    BoolExpr::Or(Box::new(left), Box::new(right))
                })
        }
    });

    let math_expr = make_math_expr(value_expr.clone());

    let math_expr_with_ops = math_expr.clone().try_map(|m, span| {
        if let MathExpr::Binary { .. } = &m {
            Ok(m)
        } else if let MathExpr::Negate(_) = &m {
            Ok(m)
        } else {
            Err(Rich::custom(span, "Expected math expression with operators"))
        }
    });

    let where_clause = just(&Token::Where).ignore_then(bool_expr.clone());

    let editor_statement = editor_call
        .then(where_clause.or_not())
        .map(|(editor, condition)| RootExpr::EditorStatement(EditorStatement { editor, condition }));

    choice((
        editor_statement.then_ignore(end()),
        math_expr_with_ops.map(RootExpr::MathExpression).then_ignore(end()),
        bool_expr.map(RootExpr::BooleanExpression).then_ignore(end()),
        math_expr.map(RootExpr::MathExpression).then_ignore(end()),
    ))
}
