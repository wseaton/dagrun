//! AST types and parser for dagrun configuration files.
//!
//! This crate provides a parser that produces semantic types with source location
//! tracking, designed to support both execution and LSP features.
//!
//! # Example
//!
//! ```
//! use dagrun_ast::parse_config;
//!
//! let source = r#"
//! # @timeout 5m
//! build:
//!     cargo build --release
//! "#;
//!
//! let config = parse_config(source).unwrap();
//! for (name, task) in &config.tasks {
//!     println!("task: {}", name);
//! }
//! ```

pub mod ast;
pub mod docs;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod semantic;
pub mod semantic_parser;
pub mod span;

// re-export syntactic AST (for LSP that needs raw spans)
pub use ast::*;
pub use error::{ParseError, ParseErrorKind};
pub use parser::parse;
pub use span::{Span, Spanned};

// re-export semantic types (for executor)
pub use semantic::{
    Config, ConfigMount, DotenvSettings, FileTransfer, K8sConfig, K8sMode, LogOutput, PortForward,
    ReadinessCheck, ServiceConfig, ServiceKind, Shebang, SshConfig, Task, TaskParameter,
};

// re-export semantic parser
pub use semantic_parser::{ParseConfigError, extract_lua_blocks, parse_config};
