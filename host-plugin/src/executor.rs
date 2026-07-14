//! SQL execution helpers (sqlx / MySqlPool)
//!
//! Executes parameterised SQL and maps results to the `SqlResult` type that
//! matches the `graphily:mysql/mysql-api` WIT interface.
//!
//! Uses `sqlx::query_with` + `MySqlArguments` for dynamic parameter binding
//! so parameter lists can be built at runtime without losing type safety.

use sqlx::{
    Column as _, Row as _,
    mysql::{MySqlArguments, MySqlPool, MySqlRow},
};
use sqlx::Arguments as _;
use serde::{Deserialize, Serialize};

// ── result type ───────────────────────────────────────────────────────────────

/// Mirrors the WIT `sql-result` record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SqlResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
    pub rows_affected: u64,
    pub last_insert_id: Option<u64>,
}

impl SqlResult {
    pub fn empty() -> Self {
        Self {
            columns: vec![],
            rows: vec![],
            rows_affected: 0,
            last_insert_id: None,
        }
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Extract all values from a MySqlRow as Option<String>.
/// Every column is decoded as a raw string; NULL maps to None.
fn row_to_strings(row: &MySqlRow) -> Vec<Option<String>> {
    (0..row.len())
        .map(|i| {
            // Try String (VARCHAR, TEXT, CHAR, DATE, DATETIME, TIMESTAMP)
            if let Ok(v) = row.try_get::<Option<String>, _>(i) {
                return v;
            }
            // Try i64 (INT, BIGINT, SMALLINT, TINYINT)
            if let Ok(v) = row.try_get::<Option<i64>, _>(i) {
                return v.map(|n| n.to_string());
            }
            // Try u64 (UNSIGNED variants)
            if let Ok(v) = row.try_get::<Option<u64>, _>(i) {
                return v.map(|n| n.to_string());
            }
            // Try f64 (FLOAT, DOUBLE, DECIMAL)
            if let Ok(v) = row.try_get::<Option<f64>, _>(i) {
                return v.map(|n| n.to_string());
            }
            // Try bool (TINYINT(1))
            if let Ok(v) = row.try_get::<Option<bool>, _>(i) {
                return v.map(|b| b.to_string());
            }
            // Try NaiveDateTime (DATETIME, TIMESTAMP binary protocol)
            if let Ok(v) = row.try_get::<Option<sqlx::types::chrono::NaiveDateTime>, _>(i) {
                return v.map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string());
            }
            // Try NaiveDate (DATE binary protocol)
            if let Ok(v) = row.try_get::<Option<sqlx::types::chrono::NaiveDate>, _>(i) {
                return v.map(|d| d.format("%Y-%m-%d").to_string());
            }
            None
        })
        .collect()
}

fn build_args(params: &[String]) -> Result<MySqlArguments, String> {
    let mut args = MySqlArguments::default();
    for p in params {
        args.add(p.as_str()).map_err(|e| format!("bind error: {e}"))?;
    }
    Ok(args)
}

// ── public executor functions ─────────────────────────────────────────────────

/// Execute a SELECT-style query. All params are positional `?` placeholders.
pub async fn execute_query(
    pool: &MySqlPool,
    sql: &str,
    params: Vec<String>,
) -> Result<SqlResult, String> {
    let args = build_args(&params)?;
    let rows: Vec<MySqlRow> = sqlx::query_with(sql, args)
        .fetch_all(pool)
        .await
        .map_err(|e| format!("query error: {e}"))?;

    let columns: Vec<String> = rows
        .first()
        .map(|r| r.columns().iter().map(|c| c.name().to_string()).collect())
        .unwrap_or_default();

    let result_rows = rows.iter().map(row_to_strings).collect();

    Ok(SqlResult {
        columns,
        rows: result_rows,
        rows_affected: 0,
        last_insert_id: None,
    })
}

/// Execute an INSERT / UPDATE / DELETE.
pub async fn execute_mutation(
    pool: &MySqlPool,
    sql: &str,
    params: Vec<String>,
) -> Result<SqlResult, String> {
    // MySQL rejects control statements via the prepared-statement protocol (error 1295).
    // Execute them as raw text queries instead.
    let sql_upper = sql.trim().to_uppercase();
    let is_control = matches!(
        sql_upper.as_str(),
        "START TRANSACTION" | "BEGIN" | "COMMIT" | "ROLLBACK" | "UNLOCK TABLES"
    ) || sql_upper.starts_with("LOCK TABLES");

    if is_control {
        sqlx::raw_sql(sql)
            .execute(pool)
            .await
            .map_err(|e| format!("mutation error: {e}"))?;
        return Ok(SqlResult::empty());
    }

    let args = build_args(&params)?;
    let result = sqlx::query_with(sql, args)
        .execute(pool)
        .await
        .map_err(|e| format!("mutation error: {e}"))?;

    let rows_affected = result.rows_affected();
    let last_insert_id = result.last_insert_id();
    let last_insert_id = if last_insert_id != 0 {
        Some(last_insert_id)
    } else {
        None
    };

    Ok(SqlResult {
        columns: vec![],
        rows: vec![],
        rows_affected,
        last_insert_id,
    })
}

/// Execute multiple statements in a single transaction.
pub async fn execute_transaction(
    pool: &MySqlPool,
    statements: Vec<(String, Vec<String>)>,
) -> Result<Vec<SqlResult>, String> {
    let mut tx = pool
        .begin()
        .await
        .map_err(|e| format!("BEGIN error: {e}"))?;

    let mut results = Vec::with_capacity(statements.len());
    for (sql, params) in statements {
        let args = build_args(&params)?;
        let result = match sqlx::query_with(sql.as_str(), args)
            .execute(&mut *tx)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                let _ = tx.rollback().await;
                return Err(format!("transaction error: {e}"));
            }
        };

        let rows_affected = result.rows_affected();
        let last_insert_id = result.last_insert_id();
        results.push(SqlResult {
            columns: vec![],
            rows: vec![],
            rows_affected,
            last_insert_id: if last_insert_id != 0 {
                Some(last_insert_id)
            } else {
                None
            },
        });
    }

    tx.commit()
        .await
        .map_err(|e| format!("COMMIT error: {e}"))?;

    Ok(results)
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sql_result_empty_constructor() {
        let r = SqlResult::empty();
        assert!(r.columns.is_empty());
        assert!(r.rows.is_empty());
        assert_eq!(r.rows_affected, 0);
        assert_eq!(r.last_insert_id, None);
    }

    #[test]
    fn sql_result_clone_and_eq() {
        let r = SqlResult {
            columns: vec!["id".to_string()],
            rows: vec![vec![Some("1".to_string())]],
            rows_affected: 1,
            last_insert_id: Some(42),
        };
        assert_eq!(r.clone(), r);
    }

    #[test]
    fn build_args_empty() {
        assert!(build_args(&[]).is_ok());
    }

    #[test]
    fn build_args_multiple() {
        assert!(build_args(&["a".to_string(), "b".to_string()]).is_ok());
    }
}
