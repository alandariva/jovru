//! The demo's assistant: canned answers, no network. Only the demo
//! (`JYKRE_DEMO`) uses it; a real tab talks to the configured provider.
use super::OPTIMIZED_SQL;
use jykre::tr;

/// The canned answer to `question` and the SQL it suggests.
pub fn answer(question: &str) -> (String, Option<&'static str>) {
    let lower = question.to_lowercase();
    if lower.contains("optimi") || lower.contains("slow") {
        (
            tr!(
                "One alternative is to aggregate the orders before the JOINs. This can reduce intermediate rows. Validate the difference with EXPLAIN ANALYZE on your database; this mock does not measure performance."
            )
            .into(),
            Some(OPTIMIZED_SQL),
        )
    } else if lower.contains("expla") {
        (
            tr!(
                "The query filters completed orders between July and September 2026, joins products and categories, and groups the values. COUNT counts orders, SUM computes revenue, and AVG returns the average ticket. The ordering shows the highest revenue first."
            )
            .into(),
            None,
        )
    } else {
        (
            tr!(
                "In this sample, Electronics accounts for 32.4% of revenue. Together with Home and decor, the two categories make up 52.7% of the total. You can explore the chart, ask for an explanation of the query, or try an optimization suggestion."
            )
            .into(),
            None,
        )
    }
}
