use std::fmt;

use serde::{Deserialize, Serialize};

/// A byte range in the condition source.
///
/// Every diagnostic carries one. SPEC.md section 17 leaves the check
/// editor's error contract open; PLAN.md answers it with spans, and this
/// is the mechanism — the editor underlines exactly these bytes.
#[derive(
  Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct Span {
  pub start: usize,
  pub end: usize,
}

impl Span {
  #[must_use]
  pub const fn new(start: usize, end: usize) -> Self {
    Self { start, end }
  }

  /// The span covering both, for errors reported against a whole
  /// subexpression.
  #[must_use]
  pub fn to(self, other: Self) -> Self {
    Self::new(self.start.min(other.start), self.end.max(other.end))
  }

  #[must_use]
  pub fn slice(self, src: &str) -> &str {
    src.get(self.start..self.end).unwrap_or("")
  }

  /// 1-based line and column of `start`, counting columns in characters.
  #[must_use]
  pub fn line_col(self, src: &str) -> (usize, usize) {
    let head = src.get(..self.start).unwrap_or(src);
    let line = head.matches('\n').count() + 1;
    let col = head
      .rsplit_once('\n')
      .map_or(head, |(_, t)| t)
      .chars()
      .count()
      + 1;
    (line, col)
  }
}

/// A parse or type error, positioned in the source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
  pub span: Span,
  pub message: String,
  /// A concrete suggestion. Shown beneath the underline in the editor.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub help: Option<String>,
}

impl Diagnostic {
  pub fn new(span: Span, message: impl Into<String>) -> Self {
    Self {
      span,
      message: message.into(),
      help: None,
    }
  }

  #[must_use]
  pub fn with_help(mut self, help: impl Into<String>) -> Self {
    self.help = Some(help.into());
    self
  }

  /// Render with the offending line and a caret underline, for the CLI
  /// and for snapshot tests. The editor uses [`Self::span`] directly.
  #[must_use]
  pub fn render(&self, src: &str) -> String {
    let (line_no, col) = self.span.line_col(src);
    let line = src.lines().nth(line_no - 1).unwrap_or("");
    let width = self.span.slice(src).chars().count().max(1);
    let mut out = format!("line {line_no}, column {col}: {}\n", self.message);
    out.push_str("  ");
    out.push_str(line);
    out.push('\n');
    out.push_str("  ");
    for _ in 1..col {
      out.push(' ');
    }
    for _ in 0..width {
      out.push('^');
    }
    if let Some(help) = &self.help {
      out.push_str("\n  help: ");
      out.push_str(help);
    }
    out
  }
}

impl fmt::Display for Diagnostic {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(&self.message)
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn line_col_is_one_based() {
    let src = "a and\nb == 1";
    assert_eq!(Span::new(0, 1).line_col(src), (1, 1));
    assert_eq!(Span::new(6, 7).line_col(src), (2, 1));
    assert_eq!(Span::new(8, 10).line_col(src), (2, 3));
  }

  #[test]
  fn render_underlines_the_span() {
    let src = "status == 1";
    let d = Diagnostic::new(Span::new(10, 11), "expected a string")
      .with_help("quote it");
    let out = d.render(src);
    assert!(out.contains("line 1, column 11"), "{out}");
    assert!(out.contains("\n            ^"), "{out}");
    assert!(out.contains("help: quote it"), "{out}");
  }

  #[test]
  fn spans_join() {
    assert_eq!(Span::new(2, 4).to(Span::new(8, 9)), Span::new(2, 9));
  }
}
