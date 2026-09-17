//! The check expression language of SPEC.md section 7.
//!
//! Deliberately small, statically typed, and deterministic. It evaluates
//! over normalized attributes, with an explicit escape hatch to the raw
//! payload, and its only clock is the sweep's `started_at`.
//!
//! The pipeline is [`parse`] → [`compile`] (type check, compile regexes)
//! → [`eval`]. A check is validated once when it is saved and evaluated
//! once per in-scope subject per sweep.

pub mod ast;
pub mod eval;
pub mod lexer;
pub mod parser;
pub mod span;
pub mod types;

pub use eval::{
  EntityAttrs, EvalCtx, EvalError, Evaluation, Primary, Subject, Tri, eval,
};
pub use parser::parse;
pub use span::{Diagnostic, Span};
pub use types::{Program, Schema, Ty, compile};
