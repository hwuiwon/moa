//! Shared bitemporal validity predicates for graph-memory reads.

use chrono::{DateTime, Utc};
use sqlx::{Postgres, QueryBuilder};

/// Appends the canonical application-time validity predicate.
///
/// With an `as_of` instant, rows are valid when `valid_from <= as_of` and
/// `valid_to` is either open or later than `as_of`. Without an `as_of` instant,
/// only currently active rows are eligible.
pub fn push_validity_filter(
    builder: &mut QueryBuilder<'_, Postgres>,
    table_alias: Option<&'static str>,
    as_of: Option<DateTime<Utc>>,
) {
    let valid_from = if let Some(alias) = table_alias {
        format!("{alias}.valid_from")
    } else {
        "valid_from".to_string()
    };
    let valid_to = if let Some(alias) = table_alias {
        format!("{alias}.valid_to")
    } else {
        "valid_to".to_string()
    };

    if let Some(as_of) = as_of {
        builder.push(valid_from);
        builder.push(" <= ");
        builder.push_bind(as_of);
        builder.push(" AND (");
        builder.push(valid_to.as_str());
        builder.push(" IS NULL OR ");
        builder.push(valid_to);
        builder.push(" > ");
        builder.push_bind(as_of);
        builder.push(")");
    } else {
        builder.push(valid_to);
        builder.push(" IS NULL");
    }
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, Utc};
    use sqlx::{Postgres, QueryBuilder};

    use super::push_validity_filter;

    fn utc(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("test timestamp should be valid RFC3339")
            .with_timezone(&Utc)
    }

    #[test]
    fn validity_predicate_sql_matches_active_rows_without_as_of() {
        // Pins: non-temporal reads keep the active-row predicate.
        let mut builder = QueryBuilder::<Postgres>::new("WHERE ");
        push_validity_filter(&mut builder, Some("node"), None);

        assert_eq!(builder.sql(), "WHERE node.valid_to IS NULL");
    }

    #[test]
    fn validity_predicate_sql_matches_bitemporal_as_of_window() {
        // Pins: temporal reads use the shared bitemporal validity window.
        let mut builder = QueryBuilder::<Postgres>::new("WHERE ");
        push_validity_filter(
            &mut builder,
            Some("node"),
            Some(utc("2026-03-11T00:00:00Z")),
        );

        assert_eq!(
            builder.sql(),
            "WHERE node.valid_from <= $1 AND (node.valid_to IS NULL OR node.valid_to > $2)"
        );
    }
}
