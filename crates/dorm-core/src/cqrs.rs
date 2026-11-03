//! Native CQRS: commands write, queries read, and the two are *physically*
//! segregated — not merely by convention (ADR 0012).
//!
//! DORM already classifies every compiled statement as a read or a write
//! (`StatementKind`, ADR 0010) and routes reads to a replica and writes to the
//! primary. CQRS is the application-level shape of that same seam:
//!
//! - A [`Command`] is a write-model unit of work. [`dispatch`] runs it **on the
//!   primary, inside a transaction** — so it is all-or-nothing and reads its own
//!   writes, and being a transaction it is pinned to the primary.
//! - A [`Query`] is a read-model operation. [`ask`] runs it **in a read-only
//!   session**, so the handler *cannot* write — a stray `INSERT` is refused by
//!   name — and its reads are served by a replica when one is configured.
//!
//! The guarantee is enforced by the engine, not promised by discipline: a query
//! that tries to write fails, and a command always lands on the primary. The two
//! halves can even run against different databases in the same process — a
//! command on the write store, a query on a read replica — by wrapping each in
//! its own [`with_session`](crate::db::with_session).
//!
//! ## Without defining a type
//!
//! [`command`] and [`query`] take a closure, for the common case where a full
//! `Command`/`Query` type is more ceremony than the operation deserves:
//!
//! ```no_run
//! # async fn f() -> dorm_core::Result<()> {
//! use dorm_core::cqrs;
//! use dorm_core::db;
//!
//! // A command: atomic, on the primary.
//! cqrs::command(|| async {
//!     db::sql_execute("update account set balance = balance - $1 where id = $2", &[]).await?;
//!     db::sql_execute("update account set balance = balance + $1 where id = $2", &[]).await
//! }).await?;
//!
//! // A query: read-only, replica-eligible.
//! let rows = cqrs::query(|| async {
//!     db::sql("select id, balance from account", &[]).await
//! }).await?;
//! # let _ = rows; Ok(()) }
//! ```

use std::future::Future;

use crate::db;
use crate::error::Result;

/// A write-model operation — the "C" in CQRS. Implement it for a typed command
/// (`TransferMoney`, `CloseAccount`) and run it with [`dispatch`], which gives it
/// the write-model guarantees: atomic, on the primary.
pub trait Command {
    /// What the command returns once applied — often `()`, or the new entity.
    type Output;

    /// Apply the command. Called by [`dispatch`] already inside a transaction on
    /// the primary; an `Err` rolls the whole command back.
    fn execute(self) -> impl Future<Output = Result<Self::Output>>;
}

/// A read-model operation — the "Q" in CQRS. Implement it for a typed query
/// (`AccountStatement`, `TopSellers`) and run it with [`ask`], which runs it
/// read-only and replica-eligible.
pub trait Query {
    /// What the query returns — a projection, a list, a count.
    type Output;

    /// Read and shape the result. Called by [`ask`] inside a read-only session,
    /// so it cannot write; its reads are served by a replica when one exists.
    fn fetch(self) -> impl Future<Output = Result<Self::Output>>;
}

/// Run a [`Command`]: atomically, on the primary.
///
/// The command is wrapped in a transaction — so a multi-statement command is
/// all-or-nothing and sees its own writes — and a transaction is always pinned to
/// the primary. Use [`command`] for the closure form.
pub async fn dispatch<C: Command>(cmd: C) -> Result<C::Output> {
    db::atomic(|_tx| cmd.execute()).await
}

/// Run a [`Query`]: read-only, replica-eligible.
///
/// The query runs inside a read-only session, so the handler physically cannot
/// write (a write is refused with [`Error::ReadOnly`](crate::Error::ReadOnly));
/// its reads route to a replica when the active [`Database`](crate::db::Database)
/// has one. Use [`query`] for the closure form.
pub async fn ask<Q: Query>(q: Q) -> Result<Q::Output> {
    db::with_read_only(true, q.fetch()).await
}

/// Run a closure as a command, without defining a [`Command`] type. Same
/// guarantees as [`dispatch`]: atomic, on the primary.
pub async fn command<T, F, Fut>(f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    db::atomic(|_tx| f()).await
}

/// Run a closure as a query, without defining a [`Query`] type. Same guarantees
/// as [`ask`]: read-only, replica-eligible.
pub async fn query<T, F, Fut>(f: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    db::with_read_only(true, f()).await
}
