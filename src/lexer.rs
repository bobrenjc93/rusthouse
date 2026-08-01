use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    Word(String),
    QuotedWord(String),
    Number(String),
    String(String),
    Comma,
    Dot,
    LeftParen,
    RightParen,
    Semicolon,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
    Eof,
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub kind: TokenKind,
    pub position: usize,
}

pub(crate) const MAX_SQL_TOKENS: usize = 2_000_000;

fn push_token(
    tokens: &mut Vec<Token>,
    kind: TokenKind,
    position: usize,
    token_limit: usize,
) -> Result<()> {
    if tokens.len() >= token_limit {
        return Err(Error::Limit {
            resource: "SQL tokens",
            limit: token_limit,
        });
    }
    tokens.push(Token { kind, position });
    Ok(())
}

pub(crate) fn lex(input: &str) -> Result<Vec<Token>> {
    lex_with_token_limit(input, MAX_SQL_TOKENS)
}

fn lex_with_token_limit(input: &str, token_limit: usize) -> Result<Vec<Token>> {
    let bytes = input.as_bytes();
    let mut position = 0;
    let mut tokens = Vec::new();
    while position < bytes.len() {
        match bytes[position] {
            byte if byte.is_ascii_whitespace() => position += 1,
            b'-' if bytes.get(position + 1) == Some(&b'-') => {
                position += 2;
                while position < bytes.len() && bytes[position] != b'\n' {
                    position += 1;
                }
            }
            b'/' if bytes.get(position + 1) == Some(&b'*') => {
                let start = position;
                position += 2;
                while position + 1 < bytes.len()
                    && !(bytes[position] == b'*' && bytes[position + 1] == b'/')
                {
                    position += 1;
                }
                if position + 1 >= bytes.len() {
                    return Err(Error::Lex {
                        position: start,
                        message: "unterminated block comment".to_owned(),
                    });
                }
                position += 2;
            }
            b'\'' => {
                let start = position;
                position += 1;
                let mut value = String::new();
                let mut closed = false;
                while position < bytes.len() {
                    match bytes[position] {
                        b'\'' if bytes.get(position + 1) == Some(&b'\'') => {
                            value.push('\'');
                            position += 2;
                        }
                        b'\'' => {
                            position += 1;
                            closed = true;
                            break;
                        }
                        b'\\' => {
                            position += 1;
                            let escaped =
                                bytes.get(position).copied().ok_or_else(|| Error::Lex {
                                    position: start,
                                    message: "unterminated string escape".to_owned(),
                                })?;
                            if escaped.is_ascii() {
                                value.push(match escaped {
                                    b'n' => '\n',
                                    b'r' => '\r',
                                    b't' => '\t',
                                    b'0' => '\0',
                                    b'\\' => '\\',
                                    b'\'' => '\'',
                                    other => other as char,
                                });
                                position += 1;
                            } else {
                                let character = input[position..]
                                    .chars()
                                    .next()
                                    .expect("escape position is a UTF-8 boundary");
                                value.push(character);
                                position += character.len_utf8();
                            }
                        }
                        byte if byte.is_ascii() => {
                            value.push(byte as char);
                            position += 1;
                        }
                        _ => {
                            let tail = &input[position..];
                            let character = tail.chars().next().expect("non-empty UTF-8 tail");
                            value.push(character);
                            position += character.len_utf8();
                        }
                    }
                }
                if !closed {
                    return Err(Error::Lex {
                        position: start,
                        message: "unterminated string literal".to_owned(),
                    });
                }
                push_token(&mut tokens, TokenKind::String(value), start, token_limit)?;
            }
            b'`' | b'"' => {
                let start = position;
                let quote = bytes[position];
                position += 1;
                let mut value = String::new();
                let mut closed = false;
                while position < bytes.len() {
                    if bytes[position] == quote {
                        if bytes.get(position + 1) == Some(&quote) {
                            value.push(quote as char);
                            position += 2;
                        } else {
                            position += 1;
                            closed = true;
                            break;
                        }
                    } else {
                        let character = input[position..]
                            .chars()
                            .next()
                            .expect("non-empty UTF-8 tail");
                        value.push(character);
                        position += character.len_utf8();
                    }
                }
                if !closed {
                    return Err(Error::Lex {
                        position: start,
                        message: "unterminated quoted identifier".to_owned(),
                    });
                }
                push_token(
                    &mut tokens,
                    TokenKind::QuotedWord(value),
                    start,
                    token_limit,
                )?;
            }
            byte if byte.is_ascii_alphabetic() || byte == b'_' => {
                let start = position;
                position += 1;
                while position < bytes.len()
                    && (bytes[position].is_ascii_alphanumeric()
                        || matches!(bytes[position], b'_' | b'$'))
                {
                    position += 1;
                }
                push_token(
                    &mut tokens,
                    TokenKind::Word(input[start..position].to_owned()),
                    start,
                    token_limit,
                )?;
            }
            byte if byte.is_ascii_digit() => {
                let start = position;
                position += 1;
                while position < bytes.len() && bytes[position].is_ascii_digit() {
                    position += 1;
                }
                if bytes.get(position) == Some(&b'.')
                    && bytes.get(position + 1).is_some_and(u8::is_ascii_digit)
                {
                    position += 1;
                    while position < bytes.len() && bytes[position].is_ascii_digit() {
                        position += 1;
                    }
                }
                if matches!(bytes.get(position), Some(b'e' | b'E')) {
                    let exponent = position;
                    position += 1;
                    if matches!(bytes.get(position), Some(b'+' | b'-')) {
                        position += 1;
                    }
                    let digits = position;
                    while position < bytes.len() && bytes[position].is_ascii_digit() {
                        position += 1;
                    }
                    if position == digits {
                        return Err(Error::Lex {
                            position: exponent,
                            message: "invalid numeric exponent".to_owned(),
                        });
                    }
                }
                push_token(
                    &mut tokens,
                    TokenKind::Number(input[start..position].to_owned()),
                    start,
                    token_limit,
                )?;
            }
            byte => {
                let start = position;
                position += 1;
                let kind = match byte {
                    b',' => TokenKind::Comma,
                    b'.' => TokenKind::Dot,
                    b'(' => TokenKind::LeftParen,
                    b')' => TokenKind::RightParen,
                    b';' => TokenKind::Semicolon,
                    b'+' => TokenKind::Plus,
                    b'-' => TokenKind::Minus,
                    b'*' => TokenKind::Star,
                    b'/' => TokenKind::Slash,
                    b'%' => TokenKind::Percent,
                    b'=' => TokenKind::Equal,
                    b'!' if bytes.get(position) == Some(&b'=') => {
                        position += 1;
                        TokenKind::NotEqual
                    }
                    b'<' if bytes.get(position) == Some(&b'=') => {
                        position += 1;
                        TokenKind::LessEqual
                    }
                    b'<' if bytes.get(position) == Some(&b'>') => {
                        position += 1;
                        TokenKind::NotEqual
                    }
                    b'<' => TokenKind::Less,
                    b'>' if bytes.get(position) == Some(&b'=') => {
                        position += 1;
                        TokenKind::GreaterEqual
                    }
                    b'>' => TokenKind::Greater,
                    _ => {
                        return Err(Error::Lex {
                            position: start,
                            message: format!("unexpected character '{}'", byte as char),
                        });
                    }
                };
                push_token(&mut tokens, kind, start, token_limit)?;
            }
        }
    }
    tokens.push(Token {
        kind: TokenKind::Eof,
        position: input.len(),
    });
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adversarial_punctuation_stops_at_the_token_budget() {
        let input = ";".repeat(4_097);
        let error = lex_with_token_limit(&input, 4_096).unwrap_err();
        assert!(matches!(
            error,
            Error::Limit {
                resource: "SQL tokens",
                limit: 4_096
            }
        ));
    }
}
