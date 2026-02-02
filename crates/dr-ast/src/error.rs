use crate::Span;

/// A parse error with location information
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub kind: ParseErrorKind,
    pub span: Span,
    pub message: String,
}

impl ParseError {
    pub fn new(kind: ParseErrorKind, span: Span, message: impl Into<String>) -> Self {
        Self {
            kind,
            span,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ParseError {}

/// Categories of parse errors
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// Unexpected character or token
    UnexpectedToken,
    /// Expected something that wasn't found
    Expected,
    /// Unclosed delimiter (backtick, brace, etc.)
    UnclosedDelimiter,
    /// Invalid annotation syntax
    InvalidAnnotation,
    /// Invalid variable assignment
    InvalidVariable,
    /// Invalid task header
    InvalidTaskHeader,
    /// Unclosed Lua block (@lua without @end)
    UnclosedLuaBlock,
    /// Annotation not followed by a task
    OrphanedAnnotation,
    /// Indentation error in task body
    IndentationError,
    /// Invalid file transfer syntax (missing colon)
    InvalidFileTransfer,
    /// Invalid key=value syntax
    InvalidKeyValue,
}

impl std::fmt::Display for ParseErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnexpectedToken => write!(f, "unexpected token"),
            Self::Expected => write!(f, "expected"),
            Self::UnclosedDelimiter => write!(f, "unclosed delimiter"),
            Self::InvalidAnnotation => write!(f, "invalid annotation"),
            Self::InvalidVariable => write!(f, "invalid variable"),
            Self::InvalidTaskHeader => write!(f, "invalid task header"),
            Self::UnclosedLuaBlock => write!(f, "unclosed lua block"),
            Self::OrphanedAnnotation => write!(f, "orphaned annotation"),
            Self::IndentationError => write!(f, "indentation error"),
            Self::InvalidFileTransfer => write!(f, "invalid file transfer"),
            Self::InvalidKeyValue => write!(f, "invalid key=value"),
        }
    }
}
