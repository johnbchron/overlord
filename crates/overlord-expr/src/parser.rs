use crate::{
  ast::{CmpOp, EntitiesOp, Expr, Lit, Path},
  lexer::{Tok, Token, lex},
  span::{Diagnostic, Span},
};

/// The functions the language knows. Listed in the error for an unknown
/// call, so an operator is never left guessing what is available.
const FUNCTIONS: [&str; 7] = [
  "count",
  "any",
  "all",
  "has_entity",
  "count_entities",
  "entity",
  "days_ago",
];

/// Parse a condition into an AST.
///
/// # Errors
/// Returns the first diagnostic. Parsing stops at the first error: unlike
/// type checking, where several independent mistakes are worth reporting
/// together, a parse error makes everything after it guesswork.
pub fn parse(src: &str) -> Result<Expr, Diagnostic> {
  let tokens = lex(src)?;
  let mut p = Parser { tokens, pos: 0 };
  if p.peek() == &Tok::Eof {
    return Err(
      Diagnostic::new(Span::new(0, src.len()), "the condition is empty")
        .with_help("a check must say what makes a subject bad"),
    );
  }
  let e = p.expr()?;
  if p.peek() != &Tok::Eof {
    let t = p.here();
    return Err(Diagnostic::new(
      t.span,
      format!("unexpected {} after the expression", t.tok.describe()),
    ));
  }
  Ok(e)
}

struct Parser {
  tokens: Vec<Token>,
  pos: usize,
}

impl Parser {
  fn here(&self) -> &Token {
    &self.tokens[self.pos]
  }

  fn peek(&self) -> &Tok {
    &self.tokens[self.pos].tok
  }

  fn span(&self) -> Span {
    self.tokens[self.pos].span
  }

  fn bump(&mut self) -> Token {
    let t = self.tokens[self.pos].clone();
    if self.pos + 1 < self.tokens.len() {
      self.pos += 1;
    }
    t
  }

  fn eat(&mut self, want: &Tok) -> bool {
    if self.peek() == want {
      self.bump();
      true
    } else {
      false
    }
  }

  fn expect(&mut self, want: &Tok) -> Result<Token, Diagnostic> {
    if self.peek() == want {
      Ok(self.bump())
    } else {
      let t = self.here();
      Err(Diagnostic::new(
        t.span,
        format!("expected `{want}`, found {}", t.tok.describe()),
      ))
    }
  }

  // or < and < not < comparison < ?? < primary
  fn expr(&mut self) -> Result<Expr, Diagnostic> {
    self.or()
  }

  fn or(&mut self) -> Result<Expr, Diagnostic> {
    let mut lhs = self.and()?;
    while self.eat(&Tok::Or) {
      let rhs = self.and()?;
      let span = lhs.span().to(rhs.span());
      lhs = Expr::Or(Box::new(lhs), Box::new(rhs), span);
    }
    Ok(lhs)
  }

  fn and(&mut self) -> Result<Expr, Diagnostic> {
    let mut lhs = self.unary_not()?;
    while self.eat(&Tok::And) {
      let rhs = self.unary_not()?;
      let span = lhs.span().to(rhs.span());
      lhs = Expr::And(Box::new(lhs), Box::new(rhs), span);
    }
    Ok(lhs)
  }

  fn unary_not(&mut self) -> Result<Expr, Diagnostic> {
    if self.peek() == &Tok::Not {
      let start = self.bump().span;
      let operand = self.unary_not()?;
      let span = start.to(operand.span());
      return Ok(Expr::Not(Box::new(operand), span));
    }
    self.comparison()
  }

  /// Comparisons do not chain: `a < b < c` is a mistake in every language
  /// that allows it, so it is rejected with a message that says so.
  fn comparison(&mut self) -> Result<Expr, Diagnostic> {
    let lhs = self.coalesce()?;

    let op = match self.peek() {
      Tok::Eq => Some(CmpOp::Eq),
      Tok::Ne => Some(CmpOp::Ne),
      Tok::Lt => Some(CmpOp::Lt),
      Tok::Le => Some(CmpOp::Le),
      Tok::Gt => Some(CmpOp::Gt),
      Tok::Ge => Some(CmpOp::Ge),
      _ => None,
    };

    let out = if let Some(op) = op {
      self.bump();
      let rhs = self.coalesce()?;
      let span = lhs.span().to(rhs.span());
      Expr::Cmp {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        span,
      }
    } else if self.eat(&Tok::In) {
      let rhs = self.coalesce()?;
      let span = lhs.span().to(rhs.span());
      Expr::In {
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        span,
      }
    } else if self.peek() == &Tok::Matches {
      self.bump();
      let t = self.bump();
      let Tok::Str(pattern) = t.tok else {
        return Err(
          Diagnostic::new(
            t.span,
            format!(
              "`matches` needs a literal pattern, found {}",
              t.tok.describe()
            ),
          )
          .with_help(
            "the pattern is compiled when the check is saved, so it \
             cannot come from the data",
          ),
        );
      };
      let span = lhs.span().to(t.span);
      Expr::Matches {
        lhs: Box::new(lhs),
        pattern,
        pat_span: t.span,
        span,
      }
    } else if self.peek() == &Tok::Is {
      let is_span = self.bump().span;
      let null = self.expect(&Tok::Null).map_err(|d| {
        d.with_help(
          "the only `is` form is `is null`; for the opposite use `exists`",
        )
      })?;
      let span = lhs.span().to(is_span.to(null.span));
      Expr::IsNull(Box::new(lhs), span)
    } else {
      return Ok(lhs);
    };

    if matches!(
      self.peek(),
      Tok::Eq | Tok::Ne | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge
    ) {
      return Err(
        Diagnostic::new(self.span(), "comparisons do not chain")
          .with_help("write `a < b and b < c`"),
      );
    }
    Ok(out)
  }

  fn coalesce(&mut self) -> Result<Expr, Diagnostic> {
    let mut lhs = self.primary()?;
    while self.eat(&Tok::Coalesce) {
      let rhs = self.primary()?;
      let span = lhs.span().to(rhs.span());
      lhs = Expr::Coalesce {
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        span,
      };
    }
    Ok(lhs)
  }

  fn primary(&mut self) -> Result<Expr, Diagnostic> {
    let t = self.here().clone();
    match t.tok {
      Tok::Num(n) => {
        self.bump();
        Ok(Expr::Lit(Lit::Num(n), t.span))
      }
      Tok::Str(ref s) => {
        self.bump();
        Ok(Expr::Lit(Lit::Str(s.clone()), t.span))
      }
      Tok::True => {
        self.bump();
        Ok(Expr::Lit(Lit::Bool(true), t.span))
      }
      Tok::False => {
        self.bump();
        Ok(Expr::Lit(Lit::Bool(false), t.span))
      }
      Tok::Null => {
        self.bump();
        Ok(Expr::Lit(Lit::Null, t.span))
      }
      Tok::Exists => {
        self.bump();
        let operand = self.primary()?;
        let span = t.span.to(operand.span());
        Ok(Expr::Exists(Box::new(operand), span))
      }
      Tok::LParen => {
        self.bump();
        let inner = self.expr()?;
        self.expect(&Tok::RParen)?;
        Ok(inner)
      }
      Tok::Ident(ref name) => {
        let name = name.clone();
        self.bump();
        if self.peek() == &Tok::LParen {
          self.call(&name, t.span)
        } else {
          Ok(Expr::Path(self.dotted(name, t.span)))
        }
      }
      Tok::Not
      | Tok::And
      | Tok::Or
      | Tok::In
      | Tok::Matches
      | Tok::Is
      | Tok::Where => Err(
        Diagnostic::new(
          t.span,
          format!("expected a value, found {}", t.tok.describe()),
        )
        .with_help("an operator needs something on both sides"),
      ),
      _ => Err(Diagnostic::new(
        t.span,
        format!("expected a value, found {}", t.tok.describe()),
      )),
    }
  }

  /// Consume a `.field.field` chain onto an already-consumed head.
  fn dotted(&mut self, head: String, head_span: Span) -> Path {
    let mut tail = Vec::new();
    let mut span = head_span;
    while self.peek() == &Tok::Dot {
      let Tok::Ident(seg) = self.tokens[self.pos + 1].tok.clone() else {
        break;
      };
      self.bump();
      let t = self.bump();
      tail.push(seg);
      span = span.to(t.span);
    }
    Path { head, tail, span }
  }

  fn call(&mut self, name: &str, name_span: Span) -> Result<Expr, Diagnostic> {
    match name {
      "count" => {
        self.expect(&Tok::LParen)?;
        let list = self.expr()?;
        let pred = self.optional_where()?;
        let close = self.expect(&Tok::RParen)?;
        Ok(Expr::Count {
          list: Box::new(list),
          pred: pred.map(Box::new),
          span: name_span.to(close.span),
        })
      }
      "any" | "all" => {
        let all = name == "all";
        self.expect(&Tok::LParen)?;
        let list = self.expr()?;
        let Some(pred) = self.optional_where()? else {
          return Err(
            Diagnostic::new(
              name_span.to(self.span()),
              format!("`{name}` needs a `where` predicate"),
            )
            .with_help(format!(
              "write `{name}(groups where external)`, or use \
               `count(...) > 0` to test for any element at all"
            )),
          );
        };
        let close = self.expect(&Tok::RParen)?;
        Ok(Expr::Quant {
          all,
          list: Box::new(list),
          pred: Box::new(pred),
          span: name_span.to(close.span),
        })
      }
      "has_entity" | "count_entities" => {
        let op = if name == "has_entity" {
          EntitiesOp::Has
        } else {
          EntitiesOp::Count
        };
        self.expect(&Tok::LParen)?;
        let (selector, sel_span) = self.selector(name)?;
        let pred = self.optional_where()?;
        let close = self.expect(&Tok::RParen)?;
        Ok(Expr::Entities {
          op,
          selector,
          sel_span,
          pred: pred.map(Box::new),
          span: name_span.to(close.span),
        })
      }
      "entity" => {
        self.expect(&Tok::LParen)?;
        let (selector, sel_span) = self.selector(name)?;
        if self.peek() == &Tok::Where {
          return Err(
            Diagnostic::new(self.span(), "`entity` takes no `where`")
              .with_help(
                "`entity(...)` is the operator-designated primary; to \
                 filter, use `has_entity(... where ...)`",
              ),
          );
        }
        let close = self.expect(&Tok::RParen)?;
        if self.peek() != &Tok::Dot {
          return Err(
            Diagnostic::new(
              name_span.to(close.span),
              "`entity(...)` must be followed by a field",
            )
            .with_help(
              "an entity cannot be compared directly; write \
               `entity(\"idp\").mfa_enrolled`",
            ),
          );
        }
        let path = self.dotted(String::new(), close.span);
        Ok(Expr::EntityField {
          selector,
          sel_span,
          span: name_span.to(path.span),
          tail: path.tail,
        })
      }
      "days_ago" => {
        self.expect(&Tok::LParen)?;
        let t = self.bump();
        let Tok::Num(days) = t.tok else {
          return Err(Diagnostic::new(
            t.span,
            format!(
              "`days_ago` needs a number of days, found {}",
              t.tok.describe()
            ),
          ));
        };
        let close = self.expect(&Tok::RParen)?;
        Ok(Expr::DaysAgo {
          days,
          span: name_span.to(close.span),
        })
      }
      other => Err(
        Diagnostic::new(name_span, format!("unknown function `{other}`"))
          .with_help(format!("known functions: {}", FUNCTIONS.join(", "))),
      ),
    }
  }

  fn optional_where(&mut self) -> Result<Option<Expr>, Diagnostic> {
    if self.eat(&Tok::Where) {
      Ok(Some(self.expr()?))
    } else {
      Ok(None)
    }
  }

  /// A selector is a string literal so it is inspectable without
  /// evaluating anything: the Rules screen can say which systems a check
  /// touches, and the type checker can reject an unknown one.
  fn selector(&mut self, fname: &str) -> Result<(String, Span), Diagnostic> {
    let t = self.bump();
    match t.tok {
      Tok::Str(s) => Ok((s, t.span)),
      other => Err(
        Diagnostic::new(
          t.span,
          format!(
            "`{fname}` needs a quoted system id or kind, found {}",
            other.describe()
          ),
        )
        .with_help(format!("write `{fname}(\"idp\")`")),
      ),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn err(src: &str) -> Diagnostic {
    parse(src).unwrap_err()
  }

  fn ok(src: &str) -> Expr {
    parse(src).unwrap_or_else(|d| panic!("{}", d.render(src)))
  }

  #[test]
  fn parses_every_example_from_the_spec() {
    for src in [
      "status == \"active\" and not mfa_enrolled",
      "has_entity(\"workspace\" where status == \"active\") and not \
       has_entity(\"idp\")",
      "is_admin and (last_login_at is null or last_login_at < days_ago(90))",
      "external_sharing == true or count(groups where external) > 0",
      "count(groups)",
      "count(groups where external)",
      "any(groups where external)",
      "all(groups where not external)",
      "count_entities(\"mdm\") > 1",
      "entity(\"idp\").mfa_enrolled",
    ] {
      ok(src);
    }
  }

  #[test]
  fn and_binds_tighter_than_or() {
    let e = ok("a or b and c");
    assert!(matches!(e, Expr::Or(..)), "{e:?}");
  }

  #[test]
  fn not_binds_looser_than_comparison() {
    // `not a == b` is `not (a == b)`, the SQL and Python reading.
    let Expr::Not(inner, _) = ok("not a == b") else {
      panic!("expected not")
    };
    assert!(matches!(*inner, Expr::Cmp { .. }));
  }

  #[test]
  fn coalesce_binds_tighter_than_comparison() {
    let Expr::Cmp { lhs, .. } = ok("a ?? 0 > 3") else {
      panic!("expected a comparison at the top")
    };
    assert!(matches!(*lhs, Expr::Coalesce { .. }));
  }

  #[test]
  fn dotted_paths_and_the_raw_escape_hatch() {
    let Expr::Path(p) = ok("raw.profile.department") else {
      panic!("expected a path")
    };
    assert!(p.is_raw());
    assert_eq!(p.source(), "raw.profile.department");
  }

  #[test]
  fn chained_comparisons_are_rejected_with_a_rewrite() {
    let d = err("a < b < c");
    assert_eq!(d.message, "comparisons do not chain");
    assert!(d.help.unwrap().contains("a < b and b < c"));
  }

  #[test]
  fn any_without_a_where_says_what_to_write_instead() {
    let d = err("any(groups)");
    assert!(d.message.contains("needs a `where`"), "{}", d.message);
    assert!(d.help.unwrap().contains("count(...) > 0"));
  }

  #[test]
  fn a_bare_entity_selector_cannot_be_compared() {
    let d = err("entity(\"idp\") == 1");
    assert!(d.message.contains("must be followed by a field"));
  }

  #[test]
  fn matches_requires_a_literal_pattern() {
    let d = err("name matches pattern_field");
    assert!(d.message.contains("literal pattern"), "{}", d.message);
    assert!(d.help.unwrap().contains("compiled when the check is saved"));
  }

  #[test]
  fn an_unknown_function_lists_the_known_ones() {
    let d = err("sum(licenses)");
    assert_eq!(d.message, "unknown function `sum`");
    assert!(d.help.unwrap().contains("count_entities"));
  }

  #[test]
  fn an_empty_condition_is_rejected() {
    assert!(err("").message.contains("empty"));
    assert!(err("   # just a comment").message.contains("empty"));
  }

  #[test]
  fn trailing_junk_is_reported_where_it_starts() {
    let d = err("mfa_enrolled mfa_enrolled");
    assert!(d.message.contains("unexpected"), "{}", d.message);
    assert_eq!(d.span.start, 13);
  }

  #[test]
  fn is_not_null_points_at_exists() {
    let d = err("last_login_at is not null");
    assert!(d.help.unwrap().contains("exists"), "{}", d.message);
  }

  #[test]
  fn unclosed_parens_are_reported() {
    assert!(err("(a and b").message.contains("expected `)`"));
  }
}
