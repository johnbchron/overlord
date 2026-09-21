//! Page chrome: the masthead, the nav, and the document shell every
//! screen is rendered into.
//!
//! maud builds this at compile time, so a malformed page is a build
//! error rather than something an operator discovers (PLAN.md section 1).
//! Escaping is the default in maud, which is why nothing here reaches
//! for `PreEscaped` outside the two places that genuinely hold markup.

use maud::{DOCTYPE, Markup, html};

use crate::{assets, auth::Identity};

/// Which nav item is the current page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
  Violations,
  Rules,
  Users,
  Entities,
  Identity,
  Sweeps,
  Systems,
  Settings,
  /// A detail page reached from elsewhere; nothing in the nav is
  /// highlighted.
  None,
}

impl Section {
  const NAV: [(Self, &'static str, &'static str); 8] = [
    (Self::Violations, "/", "Violations"),
    (Self::Rules, "/rules", "Rules"),
    (Self::Users, "/users", "Users"),
    // Next to Users deliberately: the identity queue is the work that
    // turns a list of accounts into a list of people, and it is where an
    // operator lands after seeing an `ambiguous` flag on the board.
    (Self::Identity, "/identity", "Identity"),
    // After Identity rather than beside Users: this is the list that
    // does not care whether something is a person, which is the
    // distinction the two screens before it are about.
    (Self::Entities, "/entities", "Entities"),
    (Self::Sweeps, "/sweeps", "Sweeps"),
    (Self::Systems, "/systems", "Systems"),
    (Self::Settings, "/settings", "Settings"),
  ];
}

/// A full page, with nav and the signed-in operator.
#[must_use]
pub fn page(
  identity: &Identity,
  title: &str,
  section: Section,
  content: Markup,
) -> Markup {
  shell(title, html! {
    header class="masthead" {
      a class="brand" href="/" { "overlord" }
      nav {
        @for (s, href, label) in Section::NAV {
          @if s == section {
            a href=(href) aria-current="page" { (label) }
          } @else {
            a href=(href) { (label) }
          }
        }
      }
      div class="whoami" {
        span { (identity.label()) }
        @if identity.can_sign_out() {
          a href="/auth/logout" { "Sign out" }
        }
      }
    }
    main { (content) }
  })
}

/// A page with no nav: the sign-in screen and error pages, which are
/// reachable without a session and so must not imply one.
#[must_use]
pub fn bare(title: &str, content: Markup) -> Markup {
  shell(title, html! { main { (content) } })
}

fn shell(title: &str, body: Markup) -> Markup {
  html! {
    (DOCTYPE)
    html lang="en" {
      head {
        meta charset="utf-8";
        meta name="viewport" content="width=device-width, initial-scale=1";
        title { (title) " · overlord" }
        link rel="stylesheet" href=(assets::stylesheet_path());
        script src=(assets::script_path()) defer {}
      }
      body { (body) }
    }
  }
}

/// A page heading with an optional right-hand action area.
#[must_use]
pub fn head(title: &str, lede: &str, actions: Markup) -> Markup {
  html! {
    div class="page-head" {
      div {
        h1 { (title) }
        @if !lede.is_empty() { p class="lede" { (lede) } }
      }
      div class="row shrink" { (actions) }
    }
  }
}

/// The "nothing here" state for a table body.
#[must_use]
pub fn empty(message: &str) -> Markup {
  html! { div class="empty" { (message) } }
}

/// Whether htmx issued this request rather than the browser navigating.
///
/// A screen whose filters carry `hx-push-url` has to answer both, at the
/// same URL: the fragment when htmx swaps it into the page, and the
/// whole screen when the browser asks for that URL directly — a reload,
/// a shared link, or a back-button entry htmx's history cache has
/// evicted. A URL that is pushed into the address bar but only ever
/// answers with a fragment turns the screen into its own results table
/// the moment it is loaded rather than swapped.
#[must_use]
pub fn is_htmx(headers: &axum::http::HeaderMap) -> bool {
  headers.contains_key("hx-request")
}
