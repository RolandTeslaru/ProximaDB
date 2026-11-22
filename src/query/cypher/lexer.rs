//! Cypher text to tokens. Keywords are identifiers here; the parser matches them without regard to case.

use super::CypherError;

/// The parser and the condition evaluator recurse over the query, so its length bounds the stack they need: a
/// release build aborts on a stack overflow.
const MAX_TOKENS: usize = 2_000;


#[derive(Clone, Debug, PartialEq)]
pub enum Token {
    Ident(String),
    Str(String),
    Num(f64),
    Param(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Colon,
    Comma,
    Dot,
    DotDot,
    Star,
    Pipe,
    /// `-`
    Dash,
    /// `->`
    Arrow,
    /// `<-`
    LeftArrow,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    End,
}


/// A token and the byte offset it starts at, for error messages.
pub type Spanned = (Token, usize);


pub fn tokens(text: &str) -> Result<Vec<Spanned>, CypherError> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;

    while at < bytes.len() {
        let c = text[at..]
            .chars()
            .next()
            .expect("at is a character boundary below the end");
        let start = at;

        if c.is_whitespace() {
            at += c.len_utf8();
            continue;
        }

        let next = bytes.get(at + 1).map(|&b| b as char);

        let (token, width) = match c {
            '(' => (Token::LParen, 1),
            ')' => (Token::RParen, 1),
            '[' => (Token::LBracket, 1),
            ']' => (Token::RBracket, 1),
            '{' => (Token::LBrace, 1),
            '}' => (Token::RBrace, 1),
            ':' => (Token::Colon, 1),
            ',' => (Token::Comma, 1),
            '*' => (Token::Star, 1),
            '|' => (Token::Pipe, 1),
            '=' => (Token::Eq, 1),

            '.' if next == Some('.') => (Token::DotDot, 2),
            '.' => (Token::Dot, 1),

            '-' if next == Some('>') => (Token::Arrow, 2),
            '-' => (Token::Dash, 1),

            '<' if next == Some('-') => (Token::LeftArrow, 2),
            '<' if next == Some('>') => (Token::Ne, 2),
            '<' if next == Some('=') => (Token::Le, 2),
            '<' => (Token::Lt, 1),

            '>' if next == Some('=') => (Token::Ge, 2),
            '>' => (Token::Gt, 1),

            '\'' | '"' => {
                let (text, width) = string(text, at)?;
                (Token::Str(text), width)
            }

            '$' => {
                let width = word_width(&text[at + 1..]);

                if width == 0 {
                    return Err(CypherError::at(at, "a parameter needs a name after $"));
                }

                (
                    Token::Param(text[at + 1..at + 1 + width].to_string()),
                    width + 1,
                )
            }

            '`' => {
                let close = text[at + 1..]
                    .find('`')
                    .ok_or_else(|| CypherError::at(at, "unclosed `"))?;

                (
                    Token::Ident(text[at + 1..at + 1 + close].to_string()),
                    close + 2,
                )
            }

            c if c.is_ascii_digit() => {
                let width = number_width(bytes, at);
                let number = text[at..at + width]
                    .parse()
                    .map_err(|_| CypherError::at(at, "not a number"))?;

                (Token::Num(number), width)
            }

            c if c.is_alphabetic() || c == '_' => {
                let width = word_width(&text[at..]);

                (Token::Ident(text[at..at + width].to_string()), width)
            }

            other => return Err(CypherError::at(at, format!("unexpected character {other}"))),
        };

        if out.len() == MAX_TOKENS {
            return Err(CypherError::at(
                start,
                format!("a query has at most {MAX_TOKENS} words and symbols"),
            ));
        }

        out.push((token, start));
        at += width;
    }

    out.push((Token::End, text.len()));

    Ok(out)
}


fn word_width(text: &str) -> usize {
    text.chars()
        .take_while(|&c| c.is_alphanumeric() || c == '_')
        .map(char::len_utf8)
        .sum()
}


/// Digits, then a fraction only if a digit follows the dot, so `1..4` is `1`, `..`, `4`.
fn number_width(bytes: &[u8], from: usize) -> usize {
    let digits = |at: usize| {
        bytes[at..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count()
    };

    let whole = digits(from);
    let after = from + whole;

    let has_fraction =
        bytes.get(after) == Some(&b'.') && bytes.get(after + 1).is_some_and(u8::is_ascii_digit);

    if has_fraction {
        whole + 1 + digits(after + 1)
    } else {
        whole
    }
}


/// A quoted string starting at `from`, with `\` escapes; returns its text and its width in the source.
fn string(text: &str, from: usize) -> Result<(String, usize), CypherError> {
    let quote = text.as_bytes()[from] as char;
    let mut out = String::new();
    let mut chars = text[from + 1..].char_indices();

    while let Some((offset, c)) = chars.next() {
        if c == quote {
            return Ok((out, offset + 2));
        }

        if c == '\\' {
            let (_, escaped) = chars
                .next()
                .ok_or_else(|| CypherError::at(from, "unclosed string"))?;

            out.push(match escaped {
                'n' => '\n',
                't' => '\t',
                other => other,
            });

            continue;
        }

        out.push(c);
    }

    Err(CypherError::at(from, "unclosed string"))
}
