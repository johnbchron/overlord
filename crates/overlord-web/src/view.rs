//! Shared rendering: the small vocabulary every screen draws from.
//!
//! Severity, lifecycle state, subjects and evidence appear on nearly
//! every page, and they have to look the same everywhere — a `critical`
//! badge that means one thing on the board and another on a person's
//! page would be worse than no badge. So they are built once, here.

use maud::{Markup, PreEscaped, html};
use overlord_core::{
  Evidence, Severity, SubjectRef, Timestamp, ViolationState,
};
use overlord_expr::Diagnostic;

/// A severity tier, coloured. The only saturated colour on a screen.
#[must_use]
pub fn severity(s: Severity) -> Markup {
  html! {
    span class={ "sev sev-" (s.as_str()) } { (s.as_str()) }
  }
}

/// A lifecycle state (SPEC.md section 9).
#[must_use]
pub fn state(s: ViolationState) -> Markup {
  let class = if s.counts_as_bad_state() {
    "tag tag-warn"
  } else {
    "tag"
  };
  html! { span class=(class) { (s.as_str()) } }
}

/// A timestamp. Recorded UTC throughout (SPEC.md section 13), rendered
/// short with the exact value on hover — an operator comparing a sweep
/// against a vendor's audit log needs the seconds.
#[must_use]
pub fn when(t: Timestamp) -> Markup {
  let full = t.to_string();
  let short = full.split_once('T').map_or_else(
    || full.clone(),
    |(d, rest)| format!("{d} {}", rest.get(..5).unwrap_or(rest)),
  );
  html! { span class="nowrap" title=(full) { (short) } }
}

#[must_use]
pub fn when_opt(t: Option<Timestamp>) -> Markup {
  match t {
    Some(t) => when(t),
    None => html! { span class="muted" { "—" } },
  }
}

/// The URL of a subject's detail page.
///
/// Subject refs contain `/` and, for most connectors, an email address,
/// so they travel as a query parameter rather than as path segments.
/// That keeps one encoding rule for every id overlord has instead of a
/// per-route argument about slashes.
#[must_use]
pub fn subject_href(subject: &SubjectRef) -> String {
  match subject {
    SubjectRef::Entity(e) => {
      format!("/entity?ref={}", urlencode(&e.to_string()))
    }
    SubjectRef::Person(p) => {
      format!("/person?uid={}", urlencode(p.as_str()))
    }
  }
}

/// A subject as a link, labelled the way an operator would name it.
#[must_use]
pub fn subject(subject: &SubjectRef, display_name: Option<&str>) -> Markup {
  let href = subject_href(subject);
  html! {
    a class="ref" href=(href) { (subject_label(subject, display_name)) }
  }
}

/// How a subject reads in a table cell.
#[must_use]
pub fn subject_label(
  subject: &SubjectRef,
  display_name: Option<&str>,
) -> String {
  if let Some(name) = display_name {
    return name.to_owned();
  }
  match subject {
    SubjectRef::Entity(e) => format!("{}/{}", e.system, e.entity_key),
    // An implicit person *is* an entity, so showing its raw uid would
    // put a `implicit:okta/user/...` string in front of the operator
    // where the account's own name belongs (SPEC.md section 6.4).
    SubjectRef::Person(p) => p.implicit_entity().map_or_else(
      || p.to_string(),
      |e| format!("{}/{}", e.system, e.entity_key),
    ),
  }
}

/// The captured leaf values that made a condition true (SPEC.md s7).
#[must_use]
pub fn evidence(ev: &Evidence) -> Markup {
  html! {
    @if !ev.is_empty() {
      div class="evidence" {
        @for (i, leaf) in ev.leaves.iter().enumerate() {
          @if i > 0 { ", " }
          b { (leaf.expr) } " = " (leaf.value.to_string())
        }
      }
    }
  }
}

/// The flags a violation row carries: why it might not mean what it
/// appears to (SPEC.md sections 6.4, 6.5 and 10).
#[must_use]
pub fn flags(stale: bool, ambiguous: bool, overlay_stale: bool) -> Markup {
  html! {
    @if stale {
      span class="tag tag-warn"
           title="Evaluated against last-known state on a partial sweep" {
        "stale"
      }
    }
    @if ambiguous {
      span class="tag tag-warn"
           title="A selector matched several entities with no designated \
                  primary — designate one on the person's page" {
        "ambiguous"
      }
    }
    @if overlay_stale {
      span class="tag tag-warn"
           title="Recorded against an older revision of this rule" {
        "stale overlay"
      }
    }
  }
}

/// An expression diagnostic with the offending bytes underlined.
///
/// PLAN.md section 5 answers SPEC.md section 17's open question with
/// spans, and this is where they are spent: the source line, a caret run
/// under exactly the reported bytes, and the help text beneath.
#[must_use]
pub fn diagnostic(d: &Diagnostic, src: &str) -> Markup {
  let (line_no, col) = d.span.line_col(src);
  let line = src.lines().nth(line_no - 1).unwrap_or("");
  let width = d.span.slice(src).chars().count().max(1);
  let pad = " ".repeat(col.saturating_sub(1));
  let carets = "^".repeat(width);

  html! {
    div class="diagnostic" {
      div class="msg" { "line " (line_no) ", column " (col) ": " (d.message) }
      pre {
        (line) "\n"
        span class="caret" { (PreEscaped(html_escape(&pad))) (carets) }
      }
      @if let Some(help) = &d.help {
        div class="hint" { "help: " (help) }
      }
    }
  }
}

/// Escape for the one place markup is built by hand: the caret line's
/// leading run of spaces, which has to survive as literal whitespace
/// inside a `<pre>`.
fn html_escape(s: &str) -> String {
  s.replace('&', "&amp;")
    .replace('<', "&lt;")
    .replace('>', "&gt;")
}

/// Percent-encode a query-parameter value.
///
/// Entity keys are vendor-supplied and routinely contain `@`, `+` and
/// `/`, so nothing may be assumed safe. This encodes everything outside
/// the unreserved set rather than blocklisting the characters that have
/// caused trouble so far.
#[must_use]
pub fn urlencode(s: &str) -> String {
  let mut out = String::with_capacity(s.len());
  for b in s.as_bytes() {
    match b {
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
        out.push(*b as char);
      }
      other => out.push_str(&format!("%{other:02X}")),
    }
  }
  out
}

/// A stat tile: one number and what it counts.
#[must_use]
pub fn stat(n: impl std::fmt::Display, label: &str) -> Markup {
  html! {
    div class="stat" {
      div class="n" { (n.to_string()) }
      div class="k" { (label) }
    }
  }
}

#[cfg(test)]
mod tests {
  use overlord_core::{EntityRef, PersonUid};

  use super::*;

  #[test]
  fn an_entity_key_survives_the_round_trip_into_a_url() {
    let e = EntityRef::new("gws-prod", "user", "ada+admin@example.com");
    let encoded = urlencode(&e.to_string());
    assert!(!encoded.contains('@'), "{encoded}");
    assert!(!encoded.contains('/'), "{encoded}");
    assert!(!encoded.contains('+'), "{encoded}");
  }

  #[test]
  fn an_implicit_person_is_labelled_by_its_account() {
    let e = EntityRef::new("okta-prod", "user", "ada@example.com");
    let subject = SubjectRef::Person(PersonUid::implicit(&e));
    assert_eq!(subject_label(&subject, None), "okta-prod/ada@example.com");
  }

  #[test]
  fn a_display_name_wins_when_there_is_one() {
    let e = EntityRef::new("okta-prod", "user", "ada@example.com");
    let subject = SubjectRef::Entity(e);
    assert_eq!(
      subject_label(&subject, Some("Ada Lovelace")),
      "Ada Lovelace"
    );
  }
}
