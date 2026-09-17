//! The compiler is fed operator-typed text in a web form. It must fail,
//! never panic — a panic in the check editor would take the server down
//! on a typo.

use overlord_core::SubjectKind;
use overlord_expr::{Schema, compile};
use proptest::prelude::*;

/// Fragments drawn from the language's own vocabulary, so the generator
/// spends its time on structurally plausible input rather than on text
/// the lexer rejects in the first byte.
const FRAGMENTS: &[&str] = &[
  "status",
  "mfa_enrolled",
  "groups",
  "raw.a.b",
  "count",
  "any",
  "all",
  "where",
  "has_entity",
  "count_entities",
  "entity",
  "days_ago",
  "and",
  "or",
  "not",
  "in",
  "matches",
  "is",
  "null",
  "true",
  "false",
  "exists",
  "(",
  ")",
  "==",
  "!=",
  "<",
  "<=",
  ">",
  ">=",
  "??",
  ".",
  ",",
  "\"idp\"",
  "\"active\"",
  "90",
  "0",
  "1.5",
  "#c",
  "\\",
  "\"",
  "'",
  "-",
  "[",
  "]",
  "{",
  "}",
  "\n",
  " ",
];

fn soup() -> impl Strategy<Value = String> {
  proptest::collection::vec(0..FRAGMENTS.len(), 0..24).prop_map(|idx| {
    idx
      .into_iter()
      .map(|i| FRAGMENTS[i])
      .collect::<Vec<_>>()
      .join(" ")
  })
}

proptest! {
  #![proptest_config(ProptestConfig::with_cases(2048))]

  #[test]
  fn compiling_token_soup_never_panics(src in soup()) {
    let _ = compile(&src, &Schema::new(SubjectKind::Entity));
    let _ = compile(&src, &Schema::new(SubjectKind::Person));
  }

  #[test]
  fn compiling_arbitrary_text_never_panics(src in ".{0,200}") {
    let _ = compile(&src, &Schema::new(SubjectKind::Entity));
  }

  /// Every diagnostic must point at a real byte range, because the editor
  /// slices the source with it.
  #[test]
  fn diagnostic_spans_are_in_bounds_and_on_char_boundaries(src in soup()) {
    if let Err(ds) = compile(&src, &Schema::new(SubjectKind::Person)) {
      for d in ds {
        prop_assert!(d.span.start <= d.span.end);
        prop_assert!(d.span.end <= src.len());
        prop_assert!(src.is_char_boundary(d.span.start));
        prop_assert!(src.is_char_boundary(d.span.end));
        // Rendering is what the CLI does with it; it must not panic.
        let _ = d.render(&src);
      }
    }
  }
}
