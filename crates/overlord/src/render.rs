//! Terminal output.
//!
//! Plain text, aligned, no colour. The board's job is to make the worst
//! problems obvious (SPEC.md section 8), so ordering carries the weight
//! rather than decoration.

use overlord_core::{CheckRecord, SubjectRef};
use overlord_engine::SweepOutcome;
use overlord_store::{ScoreRow, ViolationRow};

pub fn sweep(out: &SweepOutcome) {
  println!("sweep {} — {}", out.sweep, out.status.as_str());
  for s in &out.systems {
    let delta = match s.previous_count {
      Some(p) => {
        let d = s.observed_count as i64 - p as i64;
        format!("{d:+}")
      }
      None => "new".to_owned(),
    };
    print!(
      "  {:<16} {:<8} {:>5} entities ({delta})",
      s.system.as_str(),
      s.status.as_str(),
      s.observed_count
    );
    if s.tombstoned > 0 {
      print!(", {} absent", s.tombstoned);
    }
    if s.guard_tripped {
      print!(" [absence guard tripped]");
    }
    if let Some(reason) = s.completeness.reason() {
      print!(" [partial: {reason}]");
    }
    println!();
    if let Some(err) = &s.error {
      println!("      error: {err}");
    }
  }

  let e = &out.evaluation;
  println!(
    "  evaluated {} subjects: {} opened, {} regressed, {} still open, {} \
     resolved",
    e.subjects_evaluated, e.opened, e.regressed, e.standing, e.resolved
  );
  if e.expired > 0 {
    println!("  {} suppressions expired", e.expired);
  }
  if e.suggested > 0 {
    println!(
      "  {} link suggestions — nothing was linked; see `overlord suggestions`",
      e.suggested
    );
  }
  if e.carried > 0 {
    println!(
      "  {} violations carried onto the person who absorbed their subject",
      e.carried
    );
  }
  if e.ambiguous > 0 {
    println!(
      "  {} subjects were ambiguous — designate a primary entity",
      e.ambiguous
    );
  }
  for w in &out.warnings {
    println!("  warning: {w}");
  }
  for problem in &e.errors {
    match &problem.subject {
      Some(s) => {
        println!(
          "  rule problem: {} on {s}: {}",
          problem.check_id, problem.message
        );
      }
      None => {
        println!("  rule problem: {}: {}", problem.check_id, problem.message);
      }
    }
  }
}

pub fn violations(rows: &[ViolationRow]) {
  if rows.is_empty() {
    println!("nothing open");
    return;
  }
  // The read model decides what counts as new, per system. Doing it
  // here against "the latest sweep" was wrong: a sweep restricted to one
  // system emptied this section for every other system.
  let new_since: Vec<&ViolationRow> =
    rows.iter().filter(|v| v.new_since).collect();

  if !new_since.is_empty() {
    println!(
      "new since the last sweep of each system ({}):",
      new_since.len()
    );
    for v in &new_since {
      println!("  {}", line(v));
    }
    println!();
  }

  println!("all ({}):", rows.len());
  for v in rows {
    println!("  {}", line(v));
  }
}

fn line(v: &ViolationRow) -> String {
  let subject = match &v.subject {
    SubjectRef::Entity(e) => format!("{}/{}", e.system, e.entity_key),
    SubjectRef::Person(p) => p.to_string(),
  };
  let mut s = format!(
    "{:<9} {:<22} {:<40} {}",
    v.severity.as_str(),
    v.check_id.as_str(),
    subject,
    v.state.as_str()
  );
  if v.stale {
    s.push_str(" [stale]");
  }
  if v.ambiguous {
    s.push_str(" [ambiguous]");
  }
  if v.overlay_stale {
    s.push_str(" [seen under an older revision]");
  }
  if let Some(leaf) = v.evidence.leaves.first() {
    s.push_str(&format!("  ({} = {})", leaf.expr, leaf.value));
  }
  s
}

pub fn users(rows: &[ScoreRow]) {
  if rows.is_empty() {
    println!("nobody is carrying risk");
    return;
  }
  for r in rows {
    let name = r
      .display_name
      .clone()
      .unwrap_or_else(|| r.person_uid.to_string());
    let worst = r.worst_severity.map_or("-", |s| s.as_str());
    println!(
      "  {:>6}  {:<9} {:>3} violations  {}{}",
      r.score,
      worst,
      r.count,
      name,
      if r.implicit { "  (unlinked)" } else { "" }
    );
  }
}

pub fn checks(
  records: &[CheckRecord],
  counts: &[(overlord_core::CheckId, i64)],
) {
  if records.is_empty() {
    println!("no checks yet");
    return;
  }
  for c in records {
    let open = counts
      .iter()
      .find(|(id, _)| *id == c.draft.id)
      .map_or(0, |(_, n)| *n);
    println!(
      "  {:<22} r{:<4} {:<9} {:<8} {:>4} open  {}",
      c.draft.id.as_str(),
      c.revision,
      c.draft.severity.as_str(),
      if c.enabled { "enabled" } else { "disabled" },
      open,
      c.draft.name
    );
    println!("      {}", c.draft.condition);
    // A rule that has never matched anything is worth a second look,
    // but it is a prompt, not a verdict: the history to judge it lives
    // on the Rules screen (SPEC.md section 7).
    if c.enabled && open == 0 {
      println!("      (no open violations)");
    }
  }
}

pub fn dry_run(run: &overlord_engine::checks::DryRun) {
  println!(
    "{} revision {}: {} would match",
    run.check_id, run.revision, run.match_count
  );
  for sample in &run.samples {
    let subject = match &sample.subject {
      SubjectRef::Entity(e) => format!("{}/{}", e.system, e.entity_key),
      SubjectRef::Person(p) => p.to_string(),
    };
    let evidence = sample
      .evidence
      .leaves
      .iter()
      .map(|l| format!("{} = {}", l.expr, l.value))
      .collect::<Vec<_>>()
      .join(", ");
    println!("  {subject}  ({evidence})");
  }
  for e in &run.errors {
    println!("  could not evaluate: {e}");
  }
  if run.match_count > 0 {
    println!("nothing was opened; this was a dry run");
  }
}

/// Proposed links. Read-only, and it says so: an operator reading this
/// in a terminal should not have to guess whether anything happened.
pub fn suggestions(rows: &[overlord_store::Suggestion]) {
  if rows.is_empty() {
    println!(
      "no proposed links (run a sweep; suggestions are recomputed each time)"
    );
    return;
  }
  for s in rows {
    let why = match (
      s.evidence.get("field").and_then(|v| v.as_str()),
      s.evidence.get("value").and_then(|v| v.as_str()),
    ) {
      (Some(field), Some(value)) => format!("{field} = {value}"),
      _ => s.evidence.to_string(),
    };
    println!("  {:<14} {}", s.signal, s.entity);
    println!("      -> {}  ({why})", s.person_uid);
  }
  println!("nothing was linked; suggestions are never applied automatically");
}
