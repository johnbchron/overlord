use std::{
  path::{Path, PathBuf},
  sync::{
    Mutex,
    atomic::{AtomicU64, Ordering},
  },
};

use rusqlite::{Connection, OpenFlags, Transaction};
use rusqlite_migration::{M, Migrations};

use crate::error::{Result, StoreError};

/// Applied in order; each one is immutable once released.
fn migrations() -> Migrations<'static> {
  Migrations::new(vec![
    M::up(include_str!("sql/0001_initial.sql")),
    M::up(include_str!("sql/0002_system_connector.sql")),
    M::up(include_str!("sql/0003_identity_policy.sql")),
    M::up(include_str!("sql/0004_entity_search.sql")),
    M::up(include_str!("sql/0005_partial_reason.sql")),
  ])
}

/// The store handle.
///
/// One serialized writer and a pool of readers. The write model is
/// append-only with a single global sequence (PLAN.md section 3.1), so
/// serializing writes is not a limitation to be engineered around — it
/// is the thing that makes the sequence meaningful.
pub struct Db {
  source:  Source,
  writer:  Mutex<Connection>,
  readers: Mutex<Vec<Connection>>,
  /// Kept open for an in-memory database so the shared-cache database
  /// outlives any individual connection. Never used, but behind a
  /// `Mutex` all the same: a bare `Connection` is `Send` and not `Sync`,
  /// and `Db` is shared across tasks by the web server.
  _keeper: Mutex<Option<Connection>>,
}

#[derive(Clone)]
enum Source {
  File(PathBuf),
  Memory(String),
}

impl Source {
  fn open(&self) -> Result<Connection> {
    Ok(match self {
      Self::File(p) => Connection::open(p)?,
      Self::Memory(uri) => Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_WRITE
          | OpenFlags::SQLITE_OPEN_CREATE
          | OpenFlags::SQLITE_OPEN_URI
          | OpenFlags::SQLITE_OPEN_NO_MUTEX,
      )?,
    })
  }

  fn is_file(&self) -> bool { matches!(self, Self::File(_)) }
}

impl Db {
  /// Open (creating if needed) and migrate.
  ///
  /// # Errors
  /// If the file cannot be opened or a migration fails.
  pub fn open(path: impl AsRef<Path>) -> Result<Self> {
    Self::with_source(Source::File(path.as_ref().to_path_buf()))
  }

  /// A private in-memory database, for tests and dry runs.
  ///
  /// # Errors
  /// If SQLite refuses the connection or a migration fails.
  pub fn open_memory() -> Result<Self> {
    static N: AtomicU64 = AtomicU64::new(0);
    let id = N.fetch_add(1, Ordering::Relaxed);
    let uri = format!("file:overlord-mem-{id}?mode=memory&cache=shared");
    Self::with_source(Source::Memory(uri))
  }

  fn with_source(source: Source) -> Result<Self> {
    // An in-memory database lives only as long as a connection to it is
    // open, so the keeper is opened first and held for the handle's
    // lifetime.
    let keeper = if source.is_file() {
      None
    } else {
      Some(source.open()?)
    };

    let mut writer = source.open()?;
    Self::configure(&writer, source.is_file())?;
    migrations().to_latest(&mut writer)?;

    Ok(Self {
      source,
      writer: Mutex::new(writer),
      readers: Mutex::new(Vec::new()),
      _keeper: Mutex::new(keeper),
    })
  }

  fn configure(conn: &Connection, file: bool) -> Result<()> {
    if file {
      // WAL lets readers run while the single writer works. It is a
      // no-op (and an error to request) for an in-memory database.
      conn.pragma_update(None, "journal_mode", "WAL")?;
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    Ok(())
  }

  /// Run `f` inside a single transaction.
  ///
  /// SPEC.md section 13: a command is validated, appended and projected
  /// in one transaction. There is no partial application — either the
  /// stream and the projections both moved, or neither did.
  ///
  /// # Errors
  /// Whatever `f` returns; the transaction is rolled back in that case.
  pub fn write<T, E: From<StoreError>>(
    &self,
    f: impl FnOnce(&Writer<'_>) -> std::result::Result<T, E>,
  ) -> std::result::Result<T, E> {
    let mut conn = self.writer.lock().unwrap_or_else(|e| e.into_inner());
    let tx = conn.transaction().map_err(StoreError::from)?;
    let out = {
      let w = Writer { tx: &tx };
      f(&w)?
    };
    tx.commit().map_err(StoreError::from)?;
    Ok(out)
  }

  /// Run `f` against a pooled read-only connection.
  ///
  /// # Errors
  /// Whatever `f` returns, or a failure to open a connection.
  pub fn read<T, E: From<StoreError>>(
    &self,
    f: impl FnOnce(&Reader<'_>) -> std::result::Result<T, E>,
  ) -> std::result::Result<T, E> {
    let conn = {
      let mut pool = self.readers.lock().unwrap_or_else(|e| e.into_inner());
      pool.pop()
    };
    let conn = match conn {
      Some(c) => c,
      None => {
        let c = self.source.open()?;
        Self::configure(&c, false)?;
        c
      }
    };

    let out = f(&Reader { conn: &conn });

    // A connection is only returned to the pool if it is still healthy;
    // dropping it on error costs one reconnect and avoids reusing a
    // connection left mid-statement.
    if out.is_ok() {
      let mut pool = self.readers.lock().unwrap_or_else(|e| e.into_inner());
      if pool.len() < 8 {
        pool.push(conn);
      }
    }
    out
  }

  /// Drop and rebuild the projections the streams alone determine.
  ///
  /// Violations are not included: they come from evaluation, which
  /// lives in the engine. See `overlord_engine::rebuild` for a full one.
  ///
  /// # Errors
  /// If the replay fails, which means the streams hold something this
  /// binary cannot interpret.
  pub fn rebuild_projections(&self) -> Result<crate::rebuild::RebuildReport> {
    crate::rebuild::rebuild(self)
  }
}

impl std::fmt::Debug for Db {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let what = match &self.source {
      Source::File(p) => p.display().to_string(),
      Source::Memory(_) => "<memory>".to_owned(),
    };
    f.debug_struct("Db").field("source", &what).finish()
  }
}

/// A transaction with write access to the streams and projections.
pub struct Writer<'a> {
  pub(crate) tx: &'a Transaction<'a>,
}

impl Writer<'_> {
  /// Take the next `n` positions on the global stream sequence.
  ///
  /// Batched because a sweep appends thousands of facts at once, and
  /// because doing it in one statement inside the writer's transaction
  /// keeps the sequence gapless.
  ///
  /// # Errors
  /// If the sequence row is missing, which means the schema is damaged.
  pub fn take_seq(&self, n: usize) -> Result<i64> {
    let n = i64::try_from(n).unwrap_or(i64::MAX);
    let first: i64 = self.tx.query_row(
      "UPDATE stream_seq SET next = next + ?1 WHERE id = 1
       RETURNING next - ?1",
      [n],
      |r| r.get(0),
    )?;
    Ok(first)
  }

  pub(crate) fn conn(&self) -> &Transaction<'_> { self.tx }

  /// Read through this transaction, seeing its uncommitted writes.
  ///
  /// Evaluation runs inside the sweep's transaction and needs the read
  /// models — the facts it is evaluating are not committed yet.
  #[must_use]
  pub fn reader(&self) -> Reader<'_> { Reader { conn: self.tx } }
}

/// A read-only view of the store.
pub struct Reader<'a> {
  pub(crate) conn: &'a Connection,
}

impl Reader<'_> {
  /// The underlying connection.
  ///
  /// The read-only escape hatch, for ad-hoc queries the typed read
  /// models do not cover — exports, diagnostics, and tests that need to
  /// compare whole tables. Writes go through [`Writer`], never here.
  #[must_use]
  pub fn conn(&self) -> &Connection { self.conn }
}

/// Store a payload by content hash, returning the hash.
///
/// # Errors
/// On a SQLite failure.
pub fn put_payload(tx: &Transaction<'_>, body: &str) -> Result<String> {
  let hash = blake3::hash(body.as_bytes()).to_hex().to_string();
  tx.execute(
    "INSERT INTO payload (hash, body) VALUES (?1, ?2)
     ON CONFLICT (hash) DO NOTHING",
    rusqlite::params![&hash, body],
  )?;
  Ok(hash)
}

/// Read a payload back.
///
/// # Errors
/// [`StoreError::NotFound`] if the hash is unknown, which would mean a
/// dangling reference.
pub fn get_payload(conn: &Connection, hash: &str) -> Result<String> {
  conn
    .query_row("SELECT body FROM payload WHERE hash = ?1", [hash], |r| {
      r.get(0)
    })
    .map_err(|e| match e {
      rusqlite::Error::QueryReturnedNoRows => {
        StoreError::not_found(format!("payload {hash}"))
      }
      other => other.into(),
    })
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The web server shares one [`Db`] across every request task, so this
  /// is a real requirement rather than a nicety — and it is easy to lose
  /// by adding a bare `Connection` field, which is `Send` but not
  /// `Sync`.
  #[test]
  fn the_handle_can_be_shared_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Db>();
  }
}
