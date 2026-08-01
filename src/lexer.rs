use crate::error::{Error, Result};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TokenKind {
    Word(String),
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

pub(crate) fn lex(input: &str) -> Result<Vec<Token>> {
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
                tokens.push(Token {
                    kind: TokenKind::String(value),
                    position: start,
                });
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
                tokens.push(Token {
                    kind: TokenKind::Word(value),
                    position: start,
                });
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
                tokens.push(Token {
                    kind: TokenKind::Word(input[start..position].to_owned()),
                    position: start,
                });
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
                tokens.push(Token {
                    kind: TokenKind::Number(input[start..position].to_owned()),
                    position: start,
                });
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
                tokens.push(Token {
                    kind,
                    position: start,
                });
            }
        }
    }
    tokens.push(Token {
        kind: TokenKind::Eof,
        position: input.len(),
    });
    Ok(tokens)
}
