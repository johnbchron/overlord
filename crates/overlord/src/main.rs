//! The overlord binary.
//!
//! SPEC.md section 13: one binary; the web server and the CLI are two
//! front ends over the same core library. `serve` starts the UI; every
//! other verb is non-interactive so it can be scripted.

mod config;
mod render;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use overlord_connect::Registry;
use overlord_connector_fixture::FixtureConnector;
use overlord_connector_grandstream_ucm::GrandstreamUcmConnector;
use overlord_connector_gworkspace::GoogleWorkspaceConnector;
use overlord_connector_unifi_access::UnifiAccessConnector;
use overlord_core::{
  Actor, CheckDraft, CheckId, CommandKind, EntityRef, EntityType, NewCommand,
  PersonUid, SubjectRef, SuppressReason, SystemId, Timestamp, ViolationState,
};
use overlord_engine::{SweepPlan, checks, identity, run_sweep};
use overlord_store::{Db, EntityFilter};
use overlord_web::{
  AppState, auth::AuthMode, oidc::Oidc, sweeprun::SweepRunner,
};

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

  /// List collected entities, narrowed by where they came from.
  ///
  /// The Users view is person-shaped and leaves out the entity types
  /// the identity policy says are not people; this one lists everything.
  Entities {
    /// Full-text search over the latest fact's content — the
    /// normalization overlay and the vendor payload behind it.
    #[arg(long, short = 'q')]
    query:       Option<String>,
    /// Only entities read through this connector.
    #[arg(long)]
    connector:   Option<String>,
    /// Only entities in this system.
    #[arg(long)]
    system:      Option<String>,
    /// Only systems of this kind: idp, workspace, sso, mdm.
    #[arg(long)]
    kind:        Option<String>,
    /// Only entities of this type, e.g. `user` or `phone-device`.
    #[arg(long = "type")]
    entity_type: Option<String>,
    /// Include entities absent from their system's latest snapshot.
    #[arg(long)]
    all:         bool,
    /// Only those absent, which is the list of what has gone away.
    #[arg(long, conflicts_with = "all")]
    absent:      bool,
    #[arg(long, default_value = "100")]
    limit:       usize,
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

  /// Show the link suggestions this store's last sweep computed.
  ///
  /// Read-only, like the suggestions themselves: nothing here links
  /// anything (SPEC.md section 12).
  Suggestions {
    #[arg(long, default_value = "50")]
    limit: usize,
  },

  /// Attach an account to a person.
  ///
  /// `person` may be a confirmed uid, the `implicit:<account>` uid of
  /// another unlinked account — in which case a person is created and
  /// both accounts are linked to it — or be omitted, which creates a
  /// person for this account alone.
  Link {
    /// `system/entity_type/entity_key`.
    entity: String,
    person: Option<String>,
    /// A display name, when this creates the person.
    #[arg(long)]
    name:   Option<String>,
  },

  /// Detach an account from the person holding it.
  Unlink { person: String, entity: String },

  /// Combine two persons. `retired` becomes a permanent alias of
  /// `surviving` and resolves through it forever (SPEC.md section 12).
  Merge {
    surviving: String,
    retired:   String,
  },

  /// Drop every projection and replay the streams.
  Rebuild,

  /// Serve the operator UI.
  Serve {
    /// Override the configured bind address.
    #[arg(long)]
    bind:      Option<SocketAddr>,
    /// Serve a fixed principal instead of authenticating.
    ///
    /// SPEC.md section 14 requires external OIDC. This exists for local
    /// work and refuses to bind a non-loopback address, so it cannot
    /// become a deployment by accident.
    #[arg(long, value_name = "NAME")]
    dev_actor: Option<String>,
  },
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

  // Reconcile the file's identity policy into the stream before anything
  // reads it. Here rather than inside the sweep because the Users roster
  // honours the same policy, so a `serve` that never swept would
  // otherwise show a roster the configuration disagrees with. A no-op
  // when nothing changed, so it costs a read per invocation.
  overlord_engine::sync_identity_policy(
    &db,
    &cfg.identity.non_person_entity_types,
    &actor,
    now,
  )?;

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

    Command::Entities {
      query,
      connector,
      system,
      kind,
      entity_type,
      all,
      absent,
      limit,
    } => {
      let filter = EntityFilter {
        systems: system.into_iter().map(SystemId::new).collect(),
        connectors: connector.into_iter().collect(),
        // A kind that does not parse is refused rather than ignored:
        // silently listing every system because a flag was misspelled
        // is the wrong answer to `--kind mdmm`.
        kinds: match kind {
          Some(k) => vec![k.parse().with_context(|| {
            format!("--kind {k}: expected idp, workspace, sso or mdm")
          })?],
          None => Vec::new(),
        },
        entity_types: entity_type.into_iter().map(EntityType::new).collect(),
        present: if absent {
          Some(false)
        } else if all {
          None
        } else {
          Some(true)
        },
        query,
        // One more than asked for, so a full page is distinguishable
        // from a truncated one.
        limit: limit + 1,
      };
      let mut rows = db.read(|r| r.entities(&filter))?;
      let truncated = rows.len() > limit;
      rows.truncate(limit);
      render::entities(&rows, truncated);
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

    Command::Suggestions { limit } => {
      let rows = db.read(|r| r.pending_suggestions(limit))?;
      render::suggestions(&rows);
    }

    Command::Link {
      entity,
      person,
      name,
    } => {
      let entity: EntityRef = entity.parse().context("entity")?;
      // One key per invocation, as everywhere else on this path, so a
      // retried script does not link twice (SPEC.md section 6.2).
      let key = Some(format!("cli:{now}:person.link:{entity}"));
      let uid = match person {
        Some(person) => identity::confirm(
          &db,
          &actor,
          &entity,
          &PersonUid::new(person),
          None,
          now,
          key,
        )?,
        None => identity::link_to_new_person(
          &db, &actor, name, &entity, None, now, key,
        )?,
      };
      println!("{entity} -> {uid}");
    }

    Command::Unlink { person, entity } => {
      let entity: EntityRef = entity.parse().context("entity")?;
      identity::unlink(
        &db,
        &actor,
        &PersonUid::new(person),
        &entity,
        now,
        Some(format!("cli:{now}:person.unlink:{entity}")),
      )?;
      println!("unlinked {entity}");
    }

    Command::Merge { surviving, retired } => {
      let surviving = PersonUid::new(surviving);
      let retired = PersonUid::new(retired);
      identity::merge(
        &db,
        &actor,
        &surviving,
        &retired,
        now,
        Some(format!("cli:{now}:person.merge:{retired}")),
      )?;
      println!("{retired} -> {surviving}");
    }

    Command::Rebuild => {
      let report = overlord_engine::rebuild(&db)?;
      println!(
        "replayed {} commands and {} facts across {} sweeps",
        report.commands, report.facts, report.sweeps
      );
    }

    Command::Serve { bind, dev_actor } => {
      serve(cfg, &cli.config, bind, dev_actor).await?;
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

/// Every connector this binary knows about. Adding one is a line here
/// and a crate (PLAN.md section 2) — nothing else depends on the
/// concrete implementations.
fn registry() -> Registry {
  Registry::new()
    .with(FixtureConnector::boxed())
    .with(GoogleWorkspaceConnector::boxed())
    .with(UnifiAccessConnector::boxed())
    .with(GrandstreamUcmConnector::boxed())
}

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

/// Start the operator UI.
///
/// The database handle is opened again here rather than reusing the one
/// `main` made, because the server takes ownership of an `Arc` that
/// outlives this call and the CLI's handle is scoped to a single
/// command.
async fn serve(
  cfg: Config,
  config_path: &std::path::Path,
  bind: Option<SocketAddr>,
  dev_actor: Option<String>,
) -> Result<()> {
  let bind = bind.unwrap_or(cfg.server.bind);

  // OIDC wins whenever it is configured: a `--dev-actor` passed by habit
  // must never quietly downgrade a real deployment to no authentication.
  let (auth, oidc) = match (&cfg.auth, dev_actor) {
    (Some(auth), dev) => {
      if dev.is_some() {
        tracing::warn!(
          "--dev-actor was given but [auth] is configured; serving OIDC"
        );
      }
      let oidc_cfg = auth.to_oidc();
      let client = Oidc::new(oidc_cfg.clone())?;
      (AuthMode::Oidc(Box::new(oidc_cfg)), Some(client))
    }
    (None, Some(actor)) => (AuthMode::Dev { actor }, None),
    (None, None) => bail!(
      "no authentication configured: add an [auth] section to {}, or pass \
       --dev-actor NAME to serve a fixed principal on a loopback address",
      config_path.display()
    ),
  };

  let db = Arc::new(
    Db::open(&cfg.store.path)
      .with_context(|| format!("opening {}", cfg.store.path.display()))?,
  );
  let sweeps = SweepRunner::new(
    Arc::clone(&db),
    Arc::new(registry()),
    cfg.systems.clone(),
    cfg.sweep.absence_guard_pct,
  );

  let state = AppState {
    db,
    sweeps,
    // A loopback dev server is plain HTTP, so a `Secure` cookie would
    // never come back. Anything with real authentication is expected to
    // be behind TLS.
    secure: !matches!(auth, AuthMode::Dev { .. }),
    auth,
    oidc,
    config_path: config_path.display().to_string(),
  };

  overlord_web::serve(state, bind).await?;
  Ok(())
}
