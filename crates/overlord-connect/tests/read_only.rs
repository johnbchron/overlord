//! Read-only by construction, checked rather than trusted.
//!
//! PLAN.md section 5 promises two tests. The first lives in
//! `src/http.rs`: an unlisted request fails closed before any network
//! call. This is the second — a connector crate must not depend on an
//! HTTP client directly, because a connector that can build its own
//! client can reach anything, and every guarantee `RestrictedHttp`
//! makes is a guarantee about the client overlord handed it.
//!
//! Reading the manifests rather than the code is deliberate: a
//! dependency is the thing that makes the capability *available*, and it
//! is visible without compiling anything.

use std::path::{Path, PathBuf};

/// Crates that may not reach the network on their own terms.
fn connector_crates() -> Vec<PathBuf> {
  let crates = Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("crates/")
    .to_path_buf();
  let mut found: Vec<PathBuf> = std::fs::read_dir(&crates)
    .expect("the crates directory is readable")
    .filter_map(Result::ok)
    .map(|e| e.path())
    .filter(|p| {
      p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.starts_with("overlord-connector-"))
    })
    .collect();
  found.sort();
  assert!(
    !found.is_empty(),
    "no connector crates found; this test would pass vacuously"
  );
  found
}

/// Crates a connector must not name. `reqwest` is the one that matters;
/// the others are here so the next convenient client is caught too.
const FORBIDDEN: [&str; 5] =
  ["reqwest", "hyper", "ureq", "curl", "tokio-tungstenite"];

#[test]
fn a_connector_crate_cannot_build_its_own_http_client() {
  for krate in connector_crates() {
    let manifest = std::fs::read_to_string(krate.join("Cargo.toml"))
      .expect("every crate has a manifest");
    // Dependencies only: a dev-dependency (wiremock brings a server,
    // not a client a connector can reach) is a test's business.
    let deps = section(&manifest, "[dependencies]");
    for forbidden in FORBIDDEN {
      assert!(
        !names_dependency(&deps, forbidden),
        "{} depends on {forbidden}; a connector is handed a RestrictedHttp \
         and nothing else (SPEC.md section 11)",
        krate.display()
      );
    }
  }
}

#[test]
fn every_connector_crate_goes_through_overlord_connect() {
  // The other half of the same claim: a crate that neither depends on
  // an HTTP client nor on `overlord-connect` is not a connector at all,
  // and the test above would pass for the wrong reason.
  for krate in connector_crates() {
    let manifest = std::fs::read_to_string(krate.join("Cargo.toml"))
      .expect("every crate has a manifest");
    assert!(
      names_dependency(
        &section(&manifest, "[dependencies]"),
        "overlord-connect"
      ),
      "{} does not depend on overlord-connect",
      krate.display()
    );
  }
}

/// The lines of one top-level TOML section.
fn section<'a>(manifest: &'a str, heading: &str) -> Vec<&'a str> {
  manifest
    .lines()
    .skip_while(|l| l.trim() != heading)
    .skip(1)
    .take_while(|l| !l.trim_start().starts_with('['))
    .collect()
}

/// Whether a section declares a dependency on `name`, in either the
/// `name = …` or the `name.workspace = true` spelling.
fn names_dependency(lines: &[&str], name: &str) -> bool {
  lines.iter().any(|line| {
    let line = line.trim();
    line
      .strip_prefix(name)
      .is_some_and(|rest| rest.starts_with(['=', '.', ' ']))
  })
}

#[cfg(test)]
mod tests_of_the_test {
  use super::*;

  #[test]
  fn both_dependency_spellings_are_recognised() {
    let lines = vec!["reqwest.workspace = true", "serde = \"1\""];
    assert!(names_dependency(&lines, "reqwest"));
    assert!(names_dependency(&lines, "serde"));
    assert!(!names_dependency(&lines, "hyper"));
    // A prefix is not a match: `serde_json` is not `serde`.
    assert!(!names_dependency(&["serde_json = \"1\""], "serde"));
  }

  #[test]
  fn a_section_stops_at_the_next_heading() {
    let manifest =
      "[dependencies]\na = \"1\"\n\n[dev-dependencies]\nreqwest = \"1\"\n";
    let deps = section(manifest, "[dependencies]");
    assert!(names_dependency(&deps, "a"));
    assert!(!names_dependency(&deps, "reqwest"));
  }
}
