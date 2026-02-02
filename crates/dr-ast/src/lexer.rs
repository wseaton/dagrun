use crate::Span;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // structural
    Hash,        // #
    At,          // @
    ColonEquals, // :=
    Colon,       // :
    Comma,       // ,
    Equals,      // =
    Backtick,    // `
    Quote,       // "
    OpenBrace,   // {
    CloseBrace,  // }

    // content
    Identifier(String),
    Text(String), // arbitrary text content
    Indent,       // tab or 4 spaces at line start
    Newline,
    Whitespace,

    // special markers
    Shebang, // #! at start of line

    // always emitted at end
    Eof,
}

impl TokenKind {
    pub fn is_trivia(&self) -> bool {
        matches!(self, TokenKind::Whitespace | TokenKind::Newline)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    pub fn new(kind: TokenKind, span: Span) -> Self {
        Self { kind, span }
    }

    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        self.span.text(source)
    }
}

pub struct Lexer<'a> {
    source: &'a str,
    bytes: &'a [u8],
    pos: usize,
    at_line_start: bool,
}

impl<'a> Lexer<'a> {
    pub fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            at_line_start: true,
        }
    }

    pub fn tokenize(mut self) -> Vec<Token> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token();
            let is_eof = tok.kind == TokenKind::Eof;
            tokens.push(tok);
            if is_eof {
                break;
            }
        }
        tokens
    }

    fn next_token(&mut self) -> Token {
        if self.pos >= self.bytes.len() {
            return Token::new(TokenKind::Eof, Span::new(self.pos as u32, self.pos as u32));
        }

        let start = self.pos;

        // handle line start specially for indentation detection
        if self.at_line_start {
            self.at_line_start = false;

            // check for shebang at absolute start or after newline
            if self.check_str("#!") {
                self.pos += 2;
                return Token::new(TokenKind::Shebang, Span::new(start as u32, self.pos as u32));
            }

            // check for indent (tab or 2+ spaces)
            if self.peek() == Some(b'\t') {
                self.pos += 1;
                return Token::new(TokenKind::Indent, Span::new(start as u32, self.pos as u32));
            }
            if self.check_str("  ") {
                // consume all leading spaces as single indent
                while self.peek() == Some(b' ') {
                    self.pos += 1;
                }
                return Token::new(TokenKind::Indent, Span::new(start as u32, self.pos as u32));
            }
        }

        let ch = self.advance();

        match ch {
            b'\n' => {
                self.at_line_start = true;
                Token::new(TokenKind::Newline, Span::new(start as u32, self.pos as u32))
            }
            b'\r' => {
                // handle \r\n as single newline
                if self.peek() == Some(b'\n') {
                    self.pos += 1;
                }
                self.at_line_start = true;
                Token::new(TokenKind::Newline, Span::new(start as u32, self.pos as u32))
            }
            b' ' | b'\t' => {
                // consume contiguous whitespace
                while let Some(b' ' | b'\t') = self.peek() {
                    self.pos += 1;
                }
                Token::new(
                    TokenKind::Whitespace,
                    Span::new(start as u32, self.pos as u32),
                )
            }
            b'#' => {
                // check for shebang (#!) even mid-line (e.g., after indent in task body)
                if self.peek() == Some(b'!') {
                    self.pos += 1;
                    Token::new(TokenKind::Shebang, Span::new(start as u32, self.pos as u32))
                } else {
                    Token::new(TokenKind::Hash, Span::new(start as u32, self.pos as u32))
                }
            }
            b'@' => Token::new(TokenKind::At, Span::new(start as u32, self.pos as u32)),
            b':' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Token::new(
                        TokenKind::ColonEquals,
                        Span::new(start as u32, self.pos as u32),
                    )
                } else {
                    Token::new(TokenKind::Colon, Span::new(start as u32, self.pos as u32))
                }
            }
            b',' => Token::new(TokenKind::Comma, Span::new(start as u32, self.pos as u32)),
            b'=' => Token::new(TokenKind::Equals, Span::new(start as u32, self.pos as u32)),
            b'`' => Token::new(
                TokenKind::Backtick,
                Span::new(start as u32, self.pos as u32),
            ),
            b'"' => Token::new(TokenKind::Quote, Span::new(start as u32, self.pos as u32)),
            b'{' => Token::new(
                TokenKind::OpenBrace,
                Span::new(start as u32, self.pos as u32),
            ),
            b'}' => Token::new(
                TokenKind::CloseBrace,
                Span::new(start as u32, self.pos as u32),
            ),
            _ if is_ident_start(ch) => {
                while self.peek().is_some_and(is_ident_continue) {
                    self.pos += 1;
                }
                let text = &self.source[start..self.pos];
                Token::new(
                    TokenKind::Identifier(text.to_string()),
                    Span::new(start as u32, self.pos as u32),
                )
            }
            _ => {
                // consume as text until we hit a delimiter or newline
                while let Some(b) = self.peek() {
                    if is_delimiter(b) || b == b'\n' || b == b'\r' {
                        break;
                    }
                    self.pos += 1;
                }
                let text = &self.source[start..self.pos];
                Token::new(
                    TokenKind::Text(text.to_string()),
                    Span::new(start as u32, self.pos as u32),
                )
            }
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn advance(&mut self) -> u8 {
        let b = self.bytes[self.pos];
        self.pos += 1;
        b
    }

    fn check_str(&self, s: &str) -> bool {
        self.source[self.pos..].starts_with(s)
    }
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b == b'-'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.'
}

fn is_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'#' | b'@' | b':' | b',' | b'=' | b'`' | b'"' | b'{' | b'}' | b' ' | b'\t'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lex(source: &str) -> Vec<TokenKind> {
        Lexer::new(source)
            .tokenize()
            .into_iter()
            .map(|t| t.kind)
            .collect()
    }

    #[test]
    fn simple_task_header() {
        let tokens = lex("build:");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("build".to_string()),
                TokenKind::Colon,
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn variable_assignment() {
        let tokens = lex("foo := bar");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("foo".to_string()),
                TokenKind::Whitespace,
                TokenKind::ColonEquals,
                TokenKind::Whitespace,
                TokenKind::Identifier("bar".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn annotation() {
        let tokens = lex("@timeout 5m");
        assert_eq!(
            tokens,
            vec![
                TokenKind::At,
                TokenKind::Identifier("timeout".to_string()),
                TokenKind::Whitespace,
                TokenKind::Text("5m".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn indented_line() {
        let tokens = lex("build:\n\techo hello");
        assert_eq!(
            tokens,
            vec![
                TokenKind::Identifier("build".to_string()),
                TokenKind::Colon,
                TokenKind::Newline,
                TokenKind::Indent,
                TokenKind::Identifier("echo".to_string()),
                TokenKind::Whitespace,
                TokenKind::Identifier("hello".to_string()),
                TokenKind::Eof,
            ]
        );
    }

    #[test]
    fn shebang() {
        let tokens = lex("task:\n\t#!/bin/bash\n\techo hi");
        assert!(tokens.contains(&TokenKind::Shebang));
    }

    #[test]
    fn shell_expansion() {
        let tokens = lex("ver := `git rev-parse HEAD`");
        assert!(tokens.contains(&TokenKind::Backtick));
    }
}
