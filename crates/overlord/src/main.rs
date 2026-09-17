//! The overlord binary.
//!
//! SPEC.md section 13: one binary; the web server and the CLI are two
//! front ends over the same core library. The web server arrives in M2;
//! until then this is the only front end, and it is deliberately
//! non-interactive so it can be scripted.

mod config;
mod render;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use overlord_connect::Registry;
use overlord_connector_fixture::FixtureConnector;
use overlord_core::{
  Actor, CheckDraft, CheckId, CommandKind, NewCommand, SubjectRef,
  SuppressReason, SystemId, Timestamp, ViolationState,
};
use overlord_engine::{SweepPlan, checks, run_sweep};
use overlord_store::Db;

use crate::config::Config;

#[derive(Parser)]
#[command(
  name = "overlord",
  about = "Observe accounts and configuration across systems, read-only",
  version
)]
struct Cli {
  /// Configuration file.
  #[arg(long, short, default_value = "overlord.toml", global = true)]
  config: PathBuf,

  /// Who to attribute commands to. SPEC.md section 14: CLI commands
  /// record a named principal.
  #[arg(long, default_value = "cli", global = true)]
  actor: String,

  #[command(subcommand)]
  command: Command,
}

#[derive(Subcommand)]
enum Command {
  /// Collect from every configured system and evaluate.
  Sweep {
    /// Restrict to these systems. A partial sweep re-evaluates only
    /// what it covered.
    #[arg(long = "system", value_name = "ID")]
    systems: Vec<String>,
  },

  /// Work with checks.
  #[command(subcommand)]
  Checks(ChecksCmd),

  /// List violations, worst first.
  Violations {
    #[arg(long, default_value = "50")]
    limit: usize,
    /// Include suppressed, false-positive and resolved violations.
    #[arg(long)]
    all:   bool,
  },

  /// Rank subjects by risk.
  Users {
    #[arg(long, default_value = "20")]
    limit: usize,
  },

  /// Record that a violation has been seen.
  Acknowledge { check: String, subject: String },

  /// Record that a violation should not count as bad state.
  Suppress {
    check:   String,
    subject: String,
    /// accepted_risk, bad_source_data or expected.
    #[arg(long, default_value = "accepted_risk")]
    reason:  String,
    /// ISO-8601. Without it the suppression does not expire.
    #[arg(long)]
    until:   Option<String>,
  },

  /// Drop every projection and replay the streams.
  Rebuild,

  /// Row counts across the projections.
  Status,
}

#[derive(Subcommand)]
enum ChecksCmd {
  /// Show every check with its revision, state and match counts.
  List,
  /// Print every check as JSON, for review or backup.
  Export,
  /// Read checks from a JSON file and append a revision for each.
  ///
  /// SPEC.md section 15 makes the UI the only place checks are
  /// authored, and there is no UI yet (M2). This is the bootstrap for
  /// M1 and nothing more: it emits ordinary `check.upsert` commands, so
  /// the stream stays authoritative and there is no rule file to
  /// reconcile — the file is an input, never a source of truth.
  Import { file: PathBuf },
  /// Evaluate a check against current facts without opening anything.
  DryRun { check: String },
  /// Enable a check. Refused without a dry-run for its revision.
  Enable { check: String },
  /// Disable a check, resolving its open violations.
  Disable { check: String },
}

#[tokio::main]
async fn main() -> Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "overlord=info".into()),
    )
    .with_target(false)
    .init();

  let cli = Cli::parse();
  let explicit = std::env::args().any(|a| a == "--config" || a == "-c");
  let cfg = Config::load(&cli.config, explicit)?;
  let db = Db::open(&cfg.store.path)
    .with_context(|| format!("opening {}", cfg.store.path.display()))?;
  let actor = Actor::new(format!("cli:{}", cli.actor));
  let now = Timestamp::now();

  match cli.command {
    Command::Sweep { systems } => {
      if cfg.systems.is_empty() {
        bail!(
          "no systems configured; add a [[systems]] entry to {}",
          cli.config.display()
        );
      }
      let ids: Vec<SystemId> = systems.into_iter().map(SystemId::new).collect();
      let mut plan = SweepPlan::new(cfg.systems.clone()).only(&ids);
      plan.absence_guard_pct = cfg.sweep.absence_guard_pct;
      plan.actor = actor.clone();
      if plan.systems.is_empty() {
        bail!("none of the named systems are configured");
      }

      let outcome = run_sweep(&db, &registry(), &plan).await?;
      render::sweep(&outcome);
    }

    Command::Checks(cmd) => run_checks(&db, &actor, now, cmd)?,

    Command::Violations { limit, all } => {
      let states: &[ViolationState] = if all {
        &[
          ViolationState::Open,
          ViolationState::Acknowledged,
          ViolationState::Suppressed,
          ViolationState::FalsePositive,
          ViolationState::Resolved,
        ]
      } else {
        &[ViolationState::Open, ViolationState::Acknowledged]
      };
      let rows = db.read(|r| r.violations(states, limit))?;
      render::violations(&rows);
    }

    Command::Users { limit } => {
      let rows = db.read(|r| r.top_subjects(limit))?;
      render::users(&rows);
    }

    Command::Acknowledge { check, subject } => {
      let subject: SubjectRef = subject.parse().context("subject")?;
      append(&db, &actor, now, CommandKind::ViolationAcknowledge {
        check_id: CheckId::new(check),
        subject,
      })?;
      println!("acknowledged");
    }

    Command::Suppress {
      check,
      subject,
      reason,
      until,
    } => {
      let subject: SubjectRef = subject.parse().context("subject")?;
      let reason: SuppressReason = reason
        .parse()
        .context("reason must be accepted_risk, bad_source_data or expected")?;
      let until = until.map(|u| u.parse()).transpose().context("until")?;
      append(&db, &actor, now, CommandKind::ViolationSuppress {
        check_id: CheckId::new(check),
        subject,
        reason,
        until,
      })?;
      println!("suppressed");
    }

    Command::Rebuild => {
      let report = overlord_engine::rebuild(&db)?;
      println!(
        "replayed {} commands and {} facts across {} sweeps",
        report.commands, report.facts, report.sweeps
      );
    }

    Command::Status => {
      let counts = db.read(|r| r.counts())?;
      println!("entities   {}", counts.entities);
      println!(
        "persons    {}  (confirmed; unlinked accounts are implicit)",
        counts.persons
      );
      println!("checks     {}", counts.checks);
      println!("violations {}", counts.violations);
    }
  }
  Ok(())
}

fn registry() -> Registry { Registry::new().with(FixtureConnector::boxed()) }

fn append(
  db: &Db,
  actor: &Actor,
  at: Timestamp,
  kind: CommandKind,
) -> Result<()> {
  // Each invocation supplies its own idempotency key, so a retried
  // script does not act twice (SPEC.md section 14).
  let key = format!("cli:{at}:{}", kind.tag());
  let cmd = NewCommand::new(actor.clone(), kind, at).with_idempotency_key(key);
  db.write(|w| w.append_command(&cmd))?;
  Ok(())
}

fn run_checks(
  db: &Db,
  actor: &Actor,
  now: Timestamp,
  cmd: ChecksCmd,
) -> Result<()> {
  match cmd {
    ChecksCmd::List => {
      let records = db.read(|r| r.checks())?;
      let counts = db.read(|r| r.open_counts_by_check())?;
      render::checks(&records, &counts);
    }

    ChecksCmd::Export => {
      let records = db.read(|r| r.checks())?;
      println!("{}", serde_json::to_string_pretty(&records)?);
    }

    ChecksCmd::Import { file } => {
      let text = std::fs::read_to_string(&file)
        .with_context(|| format!("reading {}", file.display()))?;
      let drafts: Vec<CheckDraft> = serde_json::from_str(&text)
        .with_context(|| format!("reading {}", file.display()))?;
      for draft in &drafts {
        match checks::upsert(db, actor, draft, now, None) {
          Ok(rev) => println!("{} -> revision {rev}", draft.id),
          Err(e) => {
            // A condition that will not compile is reported against its
            // own source, with the offending span underlined.
            eprintln!("{}:\n{}", draft.id, e.render(&draft.condition));
            bail!("{} was not saved", draft.id);
          }
        }
      }
    }

    ChecksCmd::DryRun { check } => {
      let id = CheckId::new(check);
      let record = checks::current(db, &id)?;
      let run = checks::dry_run(db, actor, &id, record.revision, now)?;
      render::dry_run(&run);
    }

    ChecksCmd::Enable { check } => {
      let id = CheckId::new(check);
      let rev = checks::enable(db, actor, &id, now)?;
      println!("{id} enabled at revision {rev}");
    }

    ChecksCmd::Disable { check } => {
      let id = CheckId::new(check);
      checks::disable(db, actor, &id, now)?;
      println!("{id} disabled; its open violations are resolved");
    }
  }
  Ok(())
}
