//! Static assets, compiled into the binary and served at a
//! content-hashed path.
//!
//! PLAN.md section 1: no build step. The hash is computed once at
//! startup from the bytes themselves, so an edited stylesheet cannot be
//! served from a stale cache and an unchanged one can be cached
//! immutably forever. There is nothing to invalidate by hand.

use std::sync::LazyLock;

use axum::{
  extract::Path,
  http::{StatusCode, header},
  response::{IntoResponse, Response},
};

const CSS: &str = include_str!("../../../assets/overlord.css");
const HTMX: &str = include_str!("../../../assets/htmx.min.js");

/// One served file: its URL path, its bytes, and its media type.
struct Asset {
  path: String,
  body: &'static str,
  mime: &'static str,
}

impl Asset {
  fn new(
    stem: &str,
    ext: &str,
    body: &'static str,
    mime: &'static str,
  ) -> Self {
    // Sixteen hex characters is far more than enough to separate the
    // handful of revisions a deployment will ever serve, and keeps the
    // path readable in a browser's network tab.
    let hash = blake3::hash(body.as_bytes()).to_hex();
    Self {
      path: format!("/assets/{stem}.{}.{ext}", &hash[..16]),
      body,
      mime,
    }
  }
}

static STYLESHEET: LazyLock<Asset> =
  LazyLock::new(|| Asset::new("overlord", "css", CSS, "text/css"));

static SCRIPT: LazyLock<Asset> =
  LazyLock::new(|| Asset::new("htmx", "js", HTMX, "text/javascript"));

/// The stylesheet's current URL, for the layout's `<link>`.
#[must_use]
pub fn stylesheet_path() -> &'static str { &STYLESHEET.path }

/// htmx's current URL, for the layout's `<script>`.
#[must_use]
pub fn script_path() -> &'static str { &SCRIPT.path }

/// Serve one asset by its hashed name.
///
/// The path carries the hash, so a hit is immutable by construction and
/// a miss is a 404 rather than a stale body — there is no revalidation
/// to get wrong.
pub async fn serve(Path(name): Path<String>) -> Response {
  let requested = format!("/assets/{name}");
  for asset in [&*STYLESHEET, &*SCRIPT] {
    if asset.path == requested {
      return (
        [
          (header::CONTENT_TYPE, asset.mime),
          (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        asset.body,
      )
        .into_response();
    }
  }
  StatusCode::NOT_FOUND.into_response()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn paths_carry_a_content_hash() {
    assert!(stylesheet_path().starts_with("/assets/overlord."));
    assert!(stylesheet_path().ends_with(".css"));
    assert_ne!(stylesheet_path(), "/assets/overlord.css");
  }

  #[test]
  fn the_two_assets_do_not_collide() {
    assert_ne!(stylesheet_path(), script_path());
  }
}
