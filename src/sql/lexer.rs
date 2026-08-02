use super::ParseError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TokenKind {
    Word,
    LeftParenthesis,
    RightParenthesis,
    Comma,
    Semicolon,
    Other,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Token {
    pub(super) kind: TokenKind,
    pub(super) start: usize,
    end: usize,
}

struct Lexer<'a> {
    sql: &'a str,
    position: usize,
}

impl<'a> Lexer<'a> {
    fn new(sql: &'a str) -> Self {
        Self { sql, position: 0 }
    }

    fn next(&mut self) -> Option<Token> {
        while let Some(character) = self.current_character() {
            if !character.is_ascii_whitespace() {
                break;
            }
            self.position += character.len_utf8();
        }

        let start = self.position;
        let character = self.current_character()?;
        let kind = match character {
            '(' => TokenKind::LeftParenthesis,
            ')' => TokenKind::RightParenthesis,
            ',' => TokenKind::Comma,
            ';' => TokenKind::Semicolon,
            character if is_identifier_start(character) => {
                self.position += character.len_utf8();
                while let Some(character) = self.current_character() {
                    if !is_identifier_continue(character) {
                        break;
                    }
                    self.position += character.len_utf8();
                }
                return Some(Token {
                    kind: TokenKind::Word,
                    start,
                    end: self.position,
                });
            }
            _ => TokenKind::Other,
        };

        self.position += character.len_utf8();
        Some(Token {
            kind,
            start,
            end: self.position,
        })
    }

    fn current_character(&self) -> Option<char> {
        self.sql[self.position..].chars().next()
    }
}

const fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_'
}

const fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit()
}

pub(super) struct Parser<'a> {
    sql: &'a str,
    lexer: Lexer<'a>,
    lookahead: Option<Option<Token>>,
}

impl<'a> Parser<'a> {
    pub(super) fn new(sql: &'a str) -> Self {
        Self {
            sql,
            lexer: Lexer::new(sql),
            lookahead: None,
        }
    }

    pub(super) fn expect_keyword(&mut self, keyword: &'static str) -> Result<(), ParseError> {
        let token = self.next();
        match token {
            Some(token)
                if token.kind == TokenKind::Word
                    && self.token_text(token).eq_ignore_ascii_case(keyword) =>
            {
                Ok(())
            }
            token => Err(self.syntax_error(keyword, token)),
        }
    }

    pub(super) fn expect_identifier(
        &mut self,
        expected: &'static str,
    ) -> Result<String, ParseError> {
        self.expect_word(expected)
            .map(|token| self.token_text(token).to_owned())
    }

    pub(super) fn expect_word(&mut self, expected: &'static str) -> Result<Token, ParseError> {
        let token = self.next();
        match token {
            Some(token) if token.kind == TokenKind::Word => Ok(token),
            token => Err(self.syntax_error(expected, token)),
        }
    }

    pub(super) fn expect_kind(
        &mut self,
        kind: TokenKind,
        expected: &'static str,
    ) -> Result<Token, ParseError> {
        let token = self.next();
        match token {
            Some(token) if token.kind == kind => Ok(token),
            token => Err(self.syntax_error(expected, token)),
        }
    }

    pub(super) fn expect_text(
        &mut self,
        expected_text: &str,
        expected: &'static str,
    ) -> Result<Token, ParseError> {
        let token = self.next();
        match token {
            Some(token) if self.token_text(token) == expected_text => Ok(token),
            token => Err(self.syntax_error(expected, token)),
        }
    }

    pub(super) fn finish_statement(&mut self) -> Result<(), ParseError> {
        match self.next() {
            None => Ok(()),
            Some(token) if token.kind == TokenKind::Semicolon => match self.next() {
                None => Ok(()),
                Some(trailing) => Err(ParseError::TrailingInput {
                    position: trailing.start,
                }),
            },
            Some(token) => Err(ParseError::TrailingInput {
                position: token.start,
            }),
        }
    }

    pub(super) fn peek(&mut self) -> Option<Token> {
        if self.lookahead.is_none() {
            self.lookahead = Some(self.lexer.next());
        }
        self.lookahead.flatten()
    }

    pub(super) fn next(&mut self) -> Option<Token> {
        self.lookahead.take().unwrap_or_else(|| self.lexer.next())
    }

    pub(super) fn token_text(&self, token: Token) -> &'a str {
        &self.sql[token.start..token.end]
    }

    pub(super) fn syntax_error(&self, expected: &'static str, token: Option<Token>) -> ParseError {
        ParseError::Syntax {
            position: token.map_or(self.sql.len(), |token| token.start),
            expected,
            found: token.map(|token| self.token_text(token).to_owned()),
        }
    }
}
