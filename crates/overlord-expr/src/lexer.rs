use std::fmt;

use crate::span::{Diagnostic, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
  Ident(String),
  Str(String),
  Num(f64),
  True,
  False,
  Null,
  And,
  Or,
  Not,
  In,
  Matches,
  Is,
  Where,
  Exists,
  Eq,
  Ne,
  Lt,
  Le,
  Gt,
  Ge,
  Coalesce,
  LParen,
  RParen,
  Comma,
  Dot,
  Eof,
}

impl Tok {
  /// How the token is named in an error message.
  pub fn describe(&self) -> String {
    match self {
      Self::Ident(s) => format!("name `{s}`"),
      Self::Str(_) => "string".to_owned(),
      Self::Num(_) => "number".to_owned(),
      Self::Eof => "end of expression".to_owned(),
      other => format!("`{other}`"),
    }
  }
}

impl fmt::Display for Tok {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let s = match self {
      Self::Ident(s) => return f.write_str(s),
      Self::Str(s) => return write!(f, "{s:?}"),
      Self::Num(n) => return write!(f, "{n}"),
      Self::True => "true",
      Self::False => "false",
      Self::Null => "null",
      Self::And => "and",
      Self::Or => "or",
      Self::Not => "not",
      Self::In => "in",
      Self::Matches => "matches",
      Self::Is => "is",
      Self::Where => "where",
      Self::Exists => "exists",
      Self::Eq => "==",
      Self::Ne => "!=",
      Self::Lt => "<",
      Self::Le => "<=",
      Self::Gt => ">",
      Self::Ge => ">=",
      Self::Coalesce => "??",
      Self::LParen => "(",
      Self::RParen => ")",
      Self::Comma => ",",
      Self::Dot => ".",
      Self::Eof => "<eof>",
    };
    f.write_str(s)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
  pub tok:  Tok,
  pub span: Span,
}

/// Turn condition source into tokens.
///
/// Comments start with `#` and run to end of line, so the starter check
/// library can annotate itself (SPEC.md section 7's examples are written
/// that way).
///
/// # Errors
/// On an unterminated string, an unknown character, a malformed number,
/// or a lone `?` (which is almost always a mistyped `??`).
pub fn lex(src: &str) -> Result<Vec<Token>, Diagnostic> {
  let b = src.as_bytes();
  let mut out = Vec::new();
  let mut i = 0usize;

  while i < b.len() {
    let start = i;
    let c = b[i];

    // Whitespace and comments.
    if c.is_ascii_whitespace() {
      i += 1;
      continue;
    }
    if c == b'#' {
      while i < b.len() && b[i] != b'\n' {
        i += 1;
      }
      continue;
    }

    // Two-character operators first, so `<=` never lexes as `<` then `=`.
    let two = src.get(i..i + 2);
    let simple = match two {
      Some("==") => Some(Tok::Eq),
      Some("!=") => Some(Tok::Ne),
      Some("<=") => Some(Tok::Le),
      Some(">=") => Some(Tok::Ge),
      Some("??") => Some(Tok::Coalesce),
      _ => None,
    };
    if let Some(t) = simple {
      i += 2;
      out.push(Token {
        tok:  t,
        span: Span::new(start, i),
      });
      continue;
    }

    let single = match c {
      b'<' => Some(Tok::Lt),
      b'>' => Some(Tok::Gt),
      b'(' => Some(Tok::LParen),
      b')' => Some(Tok::RParen),
      b',' => Some(Tok::Comma),
      b'.' => Some(Tok::Dot),
      _ => None,
    };
    if let Some(t) = single {
      i += 1;
      out.push(Token {
        tok:  t,
        span: Span::new(start, i),
      });
      continue;
    }

    match c {
      b'=' => {
        return Err(
          Diagnostic::new(Span::new(start, start + 1), "unexpected `=`")
            .with_help("comparison is spelled `==`"),
        );
      }
      b'!' => {
        return Err(
          Diagnostic::new(Span::new(start, start + 1), "unexpected `!`")
            .with_help("negation is spelled `not`, inequality `!=`"),
        );
      }
      b'?' => {
        return Err(
          Diagnostic::new(Span::new(start, start + 1), "unexpected `?`")
            .with_help("the default operator is spelled `??`"),
        );
      }
      b'"' | b'\'' => {
        // Strings are scanned by character, not by byte: an escape
        // before a multi-byte character would otherwise leave the
        // cursor mid-character and the next slice would panic.
        let quote = char::from(c);
        i += 1;
        let mut s = String::new();
        let unterminated = || {
          Diagnostic::new(Span::new(start, src.len()), "unterminated string")
        };
        loop {
          let Some(ch) = src[i..].chars().next() else {
            return Err(unterminated());
          };
          if ch == quote {
            i += 1;
            break;
          }
          if ch == '\\' {
            i += 1;
            let Some(esc) = src[i..].chars().next() else {
              return Err(unterminated());
            };
            // A regex pattern is an ordinary string, so `\d` must
            // survive lexing rather than becoming an unknown escape.
            match esc {
              'n' => s.push('\n'),
              't' => s.push('\t'),
              'r' => s.push('\r'),
              '\\' => s.push('\\'),
              '"' => s.push('"'),
              '\'' => s.push('\''),
              other => {
                s.push('\\');
                s.push(other);
              }
            }
            i += esc.len_utf8();
            continue;
          }
          s.push(ch);
          i += ch.len_utf8();
        }
        out.push(Token {
          tok:  Tok::Str(s),
          span: Span::new(start, i),
        });
      }
      b'0'..=b'9' => {
        while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
          // A `.` only continues the number if a digit follows, so
          // `count(x).y` and `1.5` both lex correctly.
          if b[i] == b'.' && !b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            break;
          }
          i += 1;
        }
        let text = &src[start..i];
        let n = text.parse::<f64>().map_err(|_| {
          Diagnostic::new(
            Span::new(start, i),
            format!("`{text}` is not a valid number"),
          )
        })?;
        out.push(Token {
          tok:  Tok::Num(n),
          span: Span::new(start, i),
        });
      }
      c if c.is_ascii_alphabetic() || c == b'_' => {
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
          i += 1;
        }
        let word = &src[start..i];
        let tok = match word {
          "and" => Tok::And,
          "or" => Tok::Or,
          "not" => Tok::Not,
          "in" => Tok::In,
          "matches" => Tok::Matches,
          "is" => Tok::Is,
          "where" => Tok::Where,
          "exists" => Tok::Exists,
          "true" => Tok::True,
          "false" => Tok::False,
          "null" => Tok::Null,
          other => Tok::Ident(other.to_owned()),
        };
        out.push(Token {
          tok,
          span: Span::new(start, i),
        });
      }
      _ => {
        let ch = src[i..].chars().next().unwrap_or('\u{fffd}');
        return Err(Diagnostic::new(
          Span::new(start, start + ch.len_utf8()),
          format!("unexpected character `{ch}`"),
        ));
      }
    }
  }

  out.push(Token {
    tok:  Tok::Eof,
    span: Span::new(src.len(), src.len()),
  });
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn toks(src: &str) -> Vec<Tok> {
    lex(src).unwrap().into_iter().map(|t| t.tok).collect()
  }

  #[test]
  fn lexes_a_real_condition() {
    assert_eq!(toks("status == \"active\" and not mfa_enrolled"), [
      Tok::Ident("status".into()),
      Tok::Eq,
      Tok::Str("active".into()),
      Tok::And,
      Tok::Not,
      Tok::Ident("mfa_enrolled".into()),
      Tok::Eof,
    ]);
  }

  #[test]
  fn two_char_operators_win() {
    assert_eq!(toks("a <= b"), [
      Tok::Ident("a".into()),
      Tok::Le,
      Tok::Ident("b".into()),
      Tok::Eof
    ]);
    assert_eq!(toks("a ?? b"), [
      Tok::Ident("a".into()),
      Tok::Coalesce,
      Tok::Ident("b".into()),
      Tok::Eof
    ]);
  }

  #[test]
  fn a_dot_after_a_call_is_field_access_not_a_decimal_point() {
    assert_eq!(toks("entity(\"idp\").mfa_enrolled"), [
      Tok::Ident("entity".into()),
      Tok::LParen,
      Tok::Str("idp".into()),
      Tok::RParen,
      Tok::Dot,
      Tok::Ident("mfa_enrolled".into()),
      Tok::Eof,
    ]);
    assert_eq!(toks("1.5"), [Tok::Num(1.5), Tok::Eof]);
  }

  #[test]
  fn regex_escapes_survive_lexing() {
    assert_eq!(toks(r#"name matches "^svc-\d+$""#), [
      Tok::Ident("name".into()),
      Tok::Matches,
      Tok::Str(r"^svc-\d+$".into()),
      Tok::Eof,
    ]);
  }

  #[test]
  fn comments_run_to_end_of_line() {
    assert_eq!(toks("# a rule\nis_admin"), [
      Tok::Ident("is_admin".into()),
      Tok::Eof
    ]);
  }

  #[test]
  fn mistyped_operators_get_a_pointed_message() {
    for (src, want) in [
      ("a = 1", "comparison is spelled `==`"),
      ("!a", "negation is spelled `not`"),
      ("a ? b", "the default operator is spelled `??`"),
    ] {
      let d = lex(src).unwrap_err();
      assert!(d.help.unwrap().contains(want), "{src}");
    }
  }

  #[test]
  fn unterminated_string_is_reported_at_its_opening_quote() {
    let d = lex("name == \"ada").unwrap_err();
    assert_eq!(d.message, "unterminated string");
    assert_eq!(d.span.start, 8);
  }

  #[test]
  fn an_escape_before_a_multibyte_character_does_not_split_it() {
    // Found by the robustness property test: byte-wise escape handling
    // left the cursor mid-character and the next slice panicked.
    assert_eq!(toks("\"\\Ѩ\""), [Tok::Str("\\Ѩ".into()), Tok::Eof]);
  }

  #[test]
  fn multibyte_text_does_not_split() {
    assert_eq!(toks("\"Ada Lovelace \u{1f680}\""), [
      Tok::Str("Ada Lovelace \u{1f680}".into()),
      Tok::Eof
    ]);
  }
}
