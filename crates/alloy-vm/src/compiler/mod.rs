pub mod codegen;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod scope;
pub mod token;
pub mod typescript;

pub use codegen::Compiler;
pub use error::CompileError;
pub use scope::DEFAULT_EXPORT;
pub use token::{TemplatePart, Token, TokenStream};
pub use typescript::strip_typescript;
