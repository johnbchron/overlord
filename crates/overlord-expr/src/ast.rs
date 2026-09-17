use crate::span::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
  Eq,
  Ne,
  Lt,
  Le,
  Gt,
  Ge,
}

impl CmpOp {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Eq => "==",
      Self::Ne => "!=",
      Self::Lt => "<",
      Self::Le => "<=",
      Self::Gt => ">",
      Self::Ge => ">=",
    }
  }

  /// Ordering comparisons are the ones that need two mutually ordered
  /// operands; `==` and `!=` only need two comparable ones.
  #[must_use]
  pub fn is_ordering(self) -> bool {
    matches!(self, Self::Lt | Self::Le | Self::Gt | Self::Ge)
  }
}

/// `has_entity` and `count_entities` differ only in what they return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntitiesOp {
  Has,
  Count,
}

impl EntitiesOp {
  #[must_use]
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Has => "has_entity",
      Self::Count => "count_entities",
    }
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
  Str(String),
  Num(f64),
  Bool(bool),
  Null,
}

/// A dotted path into the subject.
///
/// `head` is a root field of the normalization overlay (SPEC.md section
/// 11 keeps it flat, so there is one place to look), or the literal
/// `raw`, which opens the escape hatch to the vendor payload. Inside a
/// `where` predicate, `head` names a field of the element instead.
#[derive(Debug, Clone, PartialEq)]
pub struct Path {
  pub head: String,
  pub tail: Vec<String>,
  pub span: Span,
}

impl Path {
  #[must_use]
  pub fn is_raw(&self) -> bool {
    self.head == "raw"
  }

  /// The source spelling, used in evidence.
  #[must_use]
  pub fn source(&self) -> String {
    let mut s = self.head.clone();
    for seg in &self.tail {
      s.push('.');
      s.push_str(seg);
    }
    s
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
  Lit(Lit, Span),
  Path(Path),
  Not(Box<Expr>, Span),
  /// `exists x` — true when `x` is not null. Never null itself, which is
  /// what makes it one of the three deliberate ways to handle absence.
  Exists(Box<Expr>, Span),
  IsNull(Box<Expr>, Span),
  And(Box<Expr>, Box<Expr>, Span),
  Or(Box<Expr>, Box<Expr>, Span),
  Cmp {
    op: CmpOp,
    lhs: Box<Expr>,
    rhs: Box<Expr>,
    span: Span,
  },
  In {
    lhs: Box<Expr>,
    rhs: Box<Expr>,
    span: Span,
  },
  /// The pattern is required to be a literal, so it compiles once at
  /// validation time and a check can never be saved with a regex that
  /// does not compile.
  Matches {
    lhs: Box<Expr>,
    pattern: String,
    pat_span: Span,
    span: Span,
  },
  Coalesce {
    lhs: Box<Expr>,
    rhs: Box<Expr>,
    span: Span,
  },
  Count {
    list: Box<Expr>,
    pred: Option<Box<Expr>>,
    span: Span,
  },
  Quant {
    /// `all` when true, `any` when false.
    all: bool,
    list: Box<Expr>,
    pred: Box<Expr>,
    span: Span,
  },
  Entities {
    op: EntitiesOp,
    selector: String,
    sel_span: Span,
    pred: Option<Box<Expr>>,
    span: Span,
  },
  /// `entity("idp").mfa_enrolled` — the primary entity for a selector.
  EntityField {
    selector: String,
    sel_span: Span,
    tail: Vec<String>,
    span: Span,
  },
  /// Derived from the sweep's `started_at`; there is no other clock.
  DaysAgo {
    days: f64,
    span: Span,
  },
}

impl Expr {
  #[must_use]
  pub fn span(&self) -> Span {
    match self {
      Self::Lit(_, s)
      | Self::Not(_, s)
      | Self::Exists(_, s)
      | Self::IsNull(_, s)
      | Self::And(_, _, s)
      | Self::Or(_, _, s)
      | Self::Cmp { span: s, .. }
      | Self::In { span: s, .. }
      | Self::Matches { span: s, .. }
      | Self::Coalesce { span: s, .. }
      | Self::Count { span: s, .. }
      | Self::Quant { span: s, .. }
      | Self::Entities { span: s, .. }
      | Self::EntityField { span: s, .. }
      | Self::DaysAgo { span: s, .. } => *s,
      Self::Path(p) => p.span,
    }
  }

  /// Whether this node is a leaf worth recording as evidence: something
  /// an operator would want to see a value for (SPEC.md section 7).
  /// Operators and connectives are not — their operands already say it.
  #[must_use]
  pub fn is_evidence_leaf(&self) -> bool {
    matches!(
      self,
      Self::Path(_)
        | Self::Count { .. }
        | Self::Quant { .. }
        | Self::Entities { .. }
        | Self::EntityField { .. }
    )
  }
}
