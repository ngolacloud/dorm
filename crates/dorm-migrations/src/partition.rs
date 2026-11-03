//! Range partitioning by month, with ahead-of-time partition creation and
//! retention (ADR 0029 / L10).
//!
//! Append-only tables — audit, consumption, events, the outbox — grow without
//! bound. Partitioning them by month keeps each partition small, lets the planner
//! prune by time, and makes retention a cheap `DETACH`/`DROP` of a whole month
//! rather than a `DELETE` of millions of rows. These helpers create the monthly
//! partitions a little ahead of time and drop the ones past the retention window.
//!
//! PostgreSQL only. The parent must be declared `PARTITION BY RANGE (<col>)`;
//! [`create_parent_ddl`] emits that. Everything here runs outside a transaction
//! (autocommit), which is what `DETACH … CONCURRENTLY` needs.

use dorm_core::db;
use dorm_core::error::{Error, Result};
use dorm_core::value::Value;

/// Reject anything that is not a plain SQL identifier before it is spliced into
/// DDL (table and schema names are configuration, not bound parameters).
fn ident(name: &str) -> Result<&str> {
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(name)
    } else {
        Err(Error::Config(format!(
            "`{name}` is not a usable SQL identifier"
        )))
    }
}

/// The `CREATE TABLE … PARTITION BY RANGE (<column>)` for a parent table. The
/// range column is part of the primary key (PostgreSQL requires the partition key
/// in every unique constraint), which is why a partitioned append-only table
/// keys on `(id, <column>)`.
pub fn create_parent_ddl(table: &str, columns_ddl: &str, range_column: &str) -> Result<String> {
    let table = ident(table)?;
    let range_column = ident(range_column)?;
    Ok(format!(
        "CREATE TABLE IF NOT EXISTS {table} ({columns_ddl}) PARTITION BY RANGE ({range_column})"
    ))
}

/// Create the monthly partitions for the current month and the next
/// `months_ahead` months, if they do not exist. Returns how many partition names
/// were ensured. Run it on a schedule so a partition always exists before the
/// month that needs it.
pub async fn ensure_month_partitions(table: &str, months_ahead: i32) -> Result<usize> {
    let table = ident(table)?;
    // The month starts to cover, computed server-side so the boundaries are the
    // database's own dates, never strings we assembled.
    let months = db::sql(
        "SELECT to_char(m, 'YYYYMM'), m::date::text, (m + interval '1 month')::date::text \
         FROM generate_series(\
             date_trunc('month', now()), \
             date_trunc('month', now()) + ($1 || ' months')::interval, \
             interval '1 month') AS m",
        &[Value::Text(months_ahead.max(0).to_string())],
    )
    .await?;

    let mut n = 0;
    for row in &months {
        let suffix = row.try_get::<String>(0)?;
        let lo = row.try_get::<String>(1)?;
        let hi = row.try_get::<String>(2)?;
        let part = format!("{table}_{suffix}");
        // `lo`/`hi` are dates the database produced; `part`/`table` are validated
        // identifiers. Nothing here comes unverified from a caller.
        db::execute(
            &format!(
                "CREATE TABLE IF NOT EXISTS {part} PARTITION OF {table} \
                 FOR VALUES FROM ('{lo}') TO ('{hi}')"
            ),
            &[],
        )
        .await?;
        n += 1;
    }
    Ok(n)
}

/// Detach and drop the monthly partitions of `table` older than `retain_months`
/// — the retention job. Returns the names dropped. Uses `DETACH … CONCURRENTLY`
/// so the parent is not locked for the detach, then drops the standalone table.
pub async fn drop_partitions_older_than(table: &str, retain_months: i32) -> Result<Vec<String>> {
    let table = ident(table)?;
    // The cutoff partition suffix: a `{table}_YYYYMM` strictly below it is past the
    // window. `YYYYMM` is fixed-width, so a lexicographic compare is a date compare.
    let cutoff = db::sql(
        "SELECT $1 || '_' || to_char(date_trunc('month', now()) - ($2 || ' months')::interval, 'YYYYMM')",
        &[
            Value::Text(table.to_string()),
            Value::Text(retain_months.max(0).to_string()),
        ],
    )
    .await?;
    let cutoff = cutoff
        .first()
        .ok_or_else(|| Error::Db("the cutoff query returned nothing".into()))?
        .try_get::<String>(0)?;

    // The expired child partitions, from the catalogue — by inheritance from the
    // parent, named below the cutoff.
    let children = db::sql(
        "SELECT child.relname FROM pg_inherits i \
         JOIN pg_class parent ON parent.oid = i.inhparent \
         JOIN pg_class child ON child.oid = i.inhrelid \
         WHERE parent.relname = $1 AND child.relname < $2 \
         ORDER BY child.relname",
        &[Value::Text(table.to_string()), Value::Text(cutoff)],
    )
    .await?;

    let mut dropped = Vec::new();
    for row in &children {
        let name = row.try_get::<String>(0)?;
        let name = ident(&name)?.to_string();
        db::execute(
            &format!("ALTER TABLE {table} DETACH PARTITION {name} CONCURRENTLY"),
            &[],
        )
        .await?;
        db::execute(&format!("DROP TABLE {name}"), &[]).await?;
        dropped.push(name);
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parent_ddl_partitions_by_range_on_the_column() {
        let ddl = create_parent_ddl(
            "events",
            "id BIGINT, at TIMESTAMPTZ NOT NULL, PRIMARY KEY (id, at)",
            "at",
        )
        .unwrap();
        assert!(ddl.contains("PARTITION BY RANGE (at)"), "{ddl}");
    }

    #[test]
    fn an_injection_in_a_table_or_column_name_is_refused() {
        assert!(create_parent_ddl("evil; DROP TABLE x", "id INT", "at").is_err());
        assert!(create_parent_ddl("events", "id INT", "at; DROP").is_err());
    }
}
