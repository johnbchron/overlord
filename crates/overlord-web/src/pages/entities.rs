//! Entities: everything overlord has collected, whatever it is.
//!
//! The Users roster (SPEC.md section 5) answers "who is there", and
//! since the identity policy landed it deliberately leaves out the
//! entity types that are not people — a fleet of handsets is not a
//! roster of persons. This is the screen that does not care: it lists
//! entities as entities, narrowed by where they came from, and searches
//! the content of the latest fact.
//!
//! The facets are the three an operator actually has in mind when
//! looking for something — which connector read it, which system it is
//! in, what kind of system that is — plus entity type, which is what
//! separates two populations read from one appliance.

use axum::{
  extract::{Query, State},
  http::HeaderMap,
  response::{Html, IntoResponse, Response},
};
use maud::{Markup, html};
use overlord_core::{EntityType, SubjectRef, SystemId, SystemKind};
use overlord_store::{EntityFilter, EntityRow};
use serde::Deserialize;

use crate::{
  AppState,
  auth::Identity,
  error::Result,
  layout::{self, Section},
  view,
};

/// How many rows the screen shows. One more is asked for, so a full
/// page can be told from a truncated one.
const LIMIT: usize = 500;

/// The facets as they travel in the URL.
///
/// One value per facet rather than a multi-select, for the reason the
/// violations board gives: repeated query keys do not round-trip through
/// `serde_urlencoded`, and a single choice per facet is what an operator
/// working a list reaches for. An absent or empty value means "all".
#[derive(Debug, Clone, Default, Deserialize)]
pub struct EntitiesQuery {
  #[serde(default)]
  pub connector:   String,
  #[serde(default)]
  pub system:      String,
  #[serde(default)]
  pub kind:        String,
  #[serde(default)]
  pub entity_type: String,
  /// `gone` shows tombstoned entities, `all` shows both.
  #[serde(default)]
  pub presence:    String,
  /// Full text, against the latest fact's content.
  #[serde(default)]
  pub q:           String,
}

impl EntitiesQuery {
  /// # Errors
  /// [`crate::error::WebError`] if a facet in the URL does not parse.
  ///
  /// Refusing rather than recovering, which is what the violations board
  /// does with a bad severity. The alternatives are both worse than a
  /// 400: ignoring the facet silently widens a filter the operator set,
  /// and defaulting it answers a question nobody asked — `?kind=mdmm`
  /// would quietly list the IdP.
  fn to_filter(&self) -> Result<EntityFilter> {
    Ok(EntityFilter {
      systems:      one(&self.system, |s| SystemId::new(s.to_owned())),
      connectors:   one(&self.connector, ToOwned::to_owned),
      kinds:        if self.kind.is_empty() {
        Vec::new()
      } else {
        vec![self.kind.parse::<SystemKind>()?]
      },
      entity_types: one(&self.entity_type, |s| EntityType::new(s.to_owned())),
      present:      match self.presence.as_str() {
        "gone" => Some(false),
        "all" => None,
        _ => Some(true),
      },
      query:        Some(self.q.clone()).filter(|q| !q.trim().is_empty()),
      limit:        LIMIT + 1,
    })
  }

  fn is_narrowed(&self) -> bool {
    !(self.connector.is_empty()
      && self.system.is_empty()
      && self.kind.is_empty()
      && self.entity_type.is_empty()
      && self.presence.is_empty()
      && self.q.trim().is_empty())
  }
}

/// An empty facet is no restriction; a set one is a list of exactly one.
fn one<T>(value: &str, f: impl Fn(&str) -> T) -> Vec<T> {
  if value.is_empty() {
    Vec::new()
  } else {
    vec![f(value)]
  }
}

pub async fn list(
  identity: Identity,
  State(state): State<AppState>,
  headers: HeaderMap,
  Query(query): Query<EntitiesQuery>,
) -> Result<Response> {
  let body = body(&state, &query)?;

  // As the Users screen: the filters push this URL into the address
  // bar, so it answers both the htmx swap and a fresh browser load of
  // the same URL.
  if layout::is_htmx(&headers) {
    return Ok(Html(body.into_string()).into_response());
  }

  let (systems, connectors, types) = state.db.read(|r| -> Result<_> {
    Ok((r.known_systems()?, r.known_connectors()?, r.entity_types()?))
  })?;

  let content = html! {
    (layout::head(
      "Entities",
      "Every object overlord has collected, in the system it came from. \
       Search runs over the content of each entity's latest fact — the \
       normalization overlay and the vendor payload behind it.",
      html! {},
    ))

    form class="filters"
         hx-get="/entities"
         hx-target="#results"
         hx-push-url="true"
         hx-trigger="change, search, keyup changed delay:300ms from:find input[name='q']" {
      label class="field" style="flex:2" {
        span { "Search" }
        input type="search" name="q" value=(query.q)
              placeholder="any value in the latest fact";
      }
      label class="field" {
        span { "Connector" }
        select name="connector" {
          option value="" selected[query.connector.is_empty()] { "Any" }
          @for c in &connectors {
            option value=(c) selected[&query.connector == c] { (c) }
          }
        }
      }
      label class="field" {
        span { "System" }
        select name="system" {
          option value="" selected[query.system.is_empty()] { "Any" }
          @for (id, _) in &systems {
            option value=(id.as_str())
                   selected[query.system == id.as_str()] { (id.as_str()) }
          }
        }
      }
      label class="field" {
        span { "Kind" }
        select name="kind" {
          option value="" selected[query.kind.is_empty()] { "Any" }
          @for k in [
            SystemKind::Idp, SystemKind::Workspace,
            SystemKind::Sso, SystemKind::Mdm,
          ] {
            option value=(k.as_str())
                   selected[query.kind == k.as_str()] { (k.as_str()) }
          }
        }
      }
      label class="field" {
        span { "Type" }
        select name="entity_type" {
          option value="" selected[query.entity_type.is_empty()] { "Any" }
          @for t in &types {
            option value=(t.as_str())
                   selected[query.entity_type == t.as_str()] { (t.as_str()) }
          }
        }
      }
      label class="field" {
        span { "Presence" }
        select name="presence" {
          option value="" selected[query.presence.is_empty()] { "Present" }
          option value="gone" selected[query.presence == "gone"] { "Absent" }
          option value="all" selected[query.presence == "all"] { "Both" }
        }
      }
      noscript { button type="submit" { "Search" } }
    }

    div id="results" { (body) }
  };

  Ok(
    Html(
      layout::page(&identity, "Entities", Section::Entities, content)
        .into_string(),
    )
    .into_response(),
  )
}

fn body(state: &AppState, query: &EntitiesQuery) -> Result<Markup> {
  let filter = query.to_filter()?;
  let mut rows: Vec<EntityRow> = state.db.read(|r| r.entities(&filter))?;
  let truncated = rows.len() > LIMIT;
  rows.truncate(LIMIT);

  Ok(html! {
    div class="panel" {
      @if rows.is_empty() {
        @if query.is_narrowed() {
          (layout::empty("Nothing matches these filters."))
        } @else {
          (layout::empty(
            "Nothing collected yet. Run a sweep: this list is every \
             object overlord has read, whether or not anything is wrong \
             with it.",
          ))
        }
      } @else {
        table {
          thead {
            tr {
              th { "Entity" }
              th class="shrink" { "Type" }
              th { "System" }
              th class="shrink" { "Connector" }
              th class="shrink" { "Kind" }
              th class="shrink" { "Status" }
              th class="shrink num" { "Violations" }
            }
          }
          tbody {
            @for r in &rows {
              @let subject = SubjectRef::Entity(r.entity.clone());
              tr {
                td {
                  (view::subject(&subject, r.display_name.as_deref()))
                  @if !r.present {
                    " " span class="tag"
                             title="Absent from the latest complete \
                                    snapshot of its system" { "absent" }
                  }
                }
                td class="shrink" {
                  span class="tag" { (r.entity.entity_type.as_str()) }
                }
                td class="ref" { (r.entity.system.as_str()) }
                td class="shrink" {
                  @match &r.connector {
                    Some(c) => span class="tag" { (c) },
                    // A system swept before connectors were recorded.
                    // Not an error, and not worth a name it does not
                    // have.
                    None => span class="muted" { "—" },
                  }
                }
                td class="shrink" {
                  @match r.system_kind {
                    Some(k) => span class="tag" { (k.as_str()) },
                    None => span class="muted" { "—" },
                  }
                }
                td class="shrink" {
                  @match r.status {
                    Some(s) => (s.as_str()),
                    // A tombstone has no overlay, so it has no status.
                    // Saying "unknown" would be a claim the store did
                    // not make.
                    None => span class="muted" { "—" },
                  }
                }
                td class="shrink num" {
                  @if r.violations > 0 {
                    b { (r.violations) }
                  } @else {
                    span class="muted" { "0" }
                  }
                }
              }
            }
          }
        }
      }
    }
    @if truncated {
      p class="muted" {
        "Showing the first " (LIMIT) ". Narrow the filters to see the rest."
      }
    }
  })
}
