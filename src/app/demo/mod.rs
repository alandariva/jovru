//! Simulated connections, queries and results for `JYKRE_DEMO=1`. Nothing
//! here touches a database.
use jykre::result::{ResultColumn, ResultData};

pub const SALES_SQL: &str = "-- Revenue by category · Q3 2026\nSELECT\n    c.name AS category,\n    COUNT(o.id) AS orders,\n    SUM(o.total) AS revenue,\n    ROUND(AVG(o.total), 2) AS avg_ticket\nFROM orders o\nJOIN products p ON p.id = o.product_id\nJOIN categories c ON c.id = p.category_id\nWHERE o.created_at >= '2026-07-01'\n  AND o.created_at <  '2026-10-01'\n  AND o.status = 'completed'\nGROUP BY c.name\nORDER BY revenue DESC;";

pub const OPTIMIZED_SQL: &str = "-- AI suggestion · aggregate before the JOINs\nWITH sales AS (\n    SELECT product_id, COUNT(*) AS orders,\n           SUM(total) AS revenue, AVG(total) AS ticket\n    FROM orders\n    WHERE created_at >= '2026-07-01'\n      AND created_at < '2026-10-01'\n      AND status = 'completed'\n    GROUP BY product_id\n)\nSELECT c.name AS category, SUM(s.orders) AS orders,\n       SUM(s.revenue) AS revenue,\n       ROUND(SUM(s.revenue) / SUM(s.orders), 2) AS avg_ticket\nFROM sales s\nJOIN products p ON p.id = s.product_id\nJOIN categories c ON c.id = p.category_id\nGROUP BY c.name ORDER BY revenue DESC;";

#[derive(Clone, Copy)]
pub struct SalesRow {
    pub category: &'static str,
    pub orders: &'static str,
    pub revenue: &'static str,
    pub ticket: &'static str,
    pub share: &'static str,
}

pub const SALES: [SalesRow; 8] = [
    SalesRow {
        category: "Electronics",
        orders: "1.284",
        revenue: "R$ 248.760,00",
        ticket: "R$ 193,74",
        share: "32,4%",
    },
    SalesRow {
        category: "Home and decor",
        orders: "986",
        revenue: "R$ 156.420,00",
        ticket: "R$ 158,64",
        share: "20,4%",
    },
    SalesRow {
        category: "Fashion and accessories",
        orders: "1.152",
        revenue: "R$ 124.680,00",
        ticket: "R$ 108,23",
        share: "16,2%",
    },
    SalesRow {
        category: "Sports and leisure",
        orders: "642",
        revenue: "R$ 89.340,00",
        ticket: "R$ 139,16",
        share: "11,6%",
    },
    SalesRow {
        category: "Health and beauty",
        orders: "874",
        revenue: "R$ 67.920,00",
        ticket: "R$ 77,71",
        share: "8,8%",
    },
    SalesRow {
        category: "Books and stationery",
        orders: "521",
        revenue: "R$ 38.550,00",
        ticket: "R$ 73,99",
        share: "5,0%",
    },
    SalesRow {
        category: "Food",
        orders: "438",
        revenue: "R$ 28.470,00",
        ticket: "R$ 65,00",
        share: "3,7%",
    },
    SalesRow {
        category: "Other",
        orders: "216",
        revenue: "R$ 14.860,00",
        ticket: "R$ 68,80",
        share: "1,9%",
    },
];

pub const TABLES: [&str; 7] = [
    "categories",
    "customers",
    "order_items",
    "orders",
    "payments",
    "products",
    "reviews",
];

pub const ORDER_COLUMNS: [(&str, &str, bool); 7] = [
    ("id", "uuid", true),
    ("customer_id", "uuid", false),
    ("product_id", "uuid", false),
    ("total", "numeric", false),
    ("status", "varchar", false),
    ("created_at", "timestamp", false),
    ("updated_at", "timestamp", false),
];

/// The application runs the demo (`JYKRE_DEMO` is set). Only then does a
/// tab without a connection run on sample data.
pub fn enabled() -> bool {
    std::env::var_os("JYKRE_DEMO").is_some()
}

/// The SQL dialect a demo connection pretends to speak, from its name.
pub fn dialect(connection: &str) -> &'static str {
    if connection.contains("Databricks") {
        "Databricks"
    } else if connection.contains("MySQL") {
        "MySQL"
    } else {
        "PostgreSQL"
    }
}

pub fn table_sql(table: &str) -> String {
    format!("-- Preview of table public.{table}\nSELECT *\nFROM public.{table}\nLIMIT 100;")
}

/// Demo rows as text; the text `NULL` stands for a NULL cell.
fn fixture<const N: usize>(
    columns: [(&'static str, &'static str); N],
    rows: &[[&str; N]],
) -> ResultData {
    ResultData::from_rows(
        columns
            .into_iter()
            .map(|(name, kind)| ResultColumn::new(name, kind))
            .collect(),
        rows.iter()
            .map(|row| row.iter().map(|cell| (*cell != "NULL").then_some(*cell))),
    )
}

/// Whether `sql` is the revenue-by-category demo, which has an assistant
/// analysis.
pub fn is_sales(sql: &str) -> bool {
    let sql = sql.to_lowercase();
    sql.contains("group by") && (sql.contains("categories") || sql.contains("category"))
}

pub fn sample_result(sql: &str) -> ResultData {
    if is_sales(sql) {
        return ResultData::from_rows(
            vec![
                ResultColumn::new("category", "text"),
                ResultColumn::new("orders", "int8"),
                ResultColumn::new("revenue", "numeric"),
                ResultColumn::new("avg_ticket", "numeric"),
                ResultColumn::new("share %", "numeric"),
            ],
            SALES.iter().map(|r| {
                [r.category, r.orders, r.revenue, r.ticket, r.share]
                    .into_iter()
                    .map(Some)
            }),
        );
    }
    let sql = sql.to_lowercase();
    if sql.contains("customers") {
        fixture(
            [
                ("id", "int8"),
                ("name", "varchar"),
                ("city", "varchar"),
                ("orders", "int8"),
                ("created_at", "date"),
            ],
            &[
                ["1001", "Marina Costa", "São Paulo", "12", "2026-07-03"],
                ["1002", "Rafael Lima", "Curitiba", "8", "2026-07-08"],
                [
                    "1003",
                    "Camila Santos",
                    "Belo Horizonte",
                    "21",
                    "2026-07-12",
                ],
                ["1004", "Lucas Ferreira", "Porto Alegre", "5", "2026-08-01"],
                ["1005", "Beatriz Almeida", "Recife", "14", "2026-08-19"],
                ["1006", "Pedro Rocha", "Campinas", "9", "2026-09-02"],
            ],
        )
    } else if sql.contains("events") {
        fixture(
            [
                ("day", "date"),
                ("events", "int8"),
                ("active_users", "int8"),
            ],
            &[
                ["2026-09-30", "28.410", "3.215"],
                ["2026-09-29", "26.985", "3.084"],
                ["2026-09-28", "31.204", "3.586"],
                ["2026-09-27", "22.150", "2.910"],
                ["2026-09-26", "25.670", "3.102"],
            ],
        )
    } else if sql.contains("transactions") {
        fixture(
            [("month", "varchar"), ("total", "numeric")],
            &[
                ["2026-09", "R$ 281.350,00"],
                ["2026-08", "R$ 254.780,00"],
                ["2026-07", "R$ 232.870,00"],
            ],
        )
    } else if sql.contains("products") {
        fixture(
            [
                ("id", "int8"),
                ("name", "varchar"),
                ("category_id", "int8"),
                ("price", "numeric"),
                ("stock", "int8"),
            ],
            &[
                ["101", "Wireless headphones", "1", "R$ 189,90", "248"],
                ["102", "Desk lamp", "2", "R$ 129,90", "86"],
                ["103", "City backpack", "3", "R$ 159,00", "124"],
                ["104", "Thermal bottle", "4", "R$ 79,90", "351"],
                ["105", "Mechanical keyboard", "1", "R$ 349,00", "62"],
            ],
        )
    } else if sql.contains("categories") {
        fixture(
            [("id", "int8"), ("name", "varchar"), ("active", "bool")],
            &[
                ["1", "Electronics", "true"],
                ["2", "Home and decor", "true"],
                ["3", "Fashion and accessories", "true"],
                ["4", "Sports and leisure", "true"],
                ["5", "Health and beauty", "true"],
                ["6", "Books and stationery", "true"],
            ],
        )
    } else if sql.contains("payments") {
        fixture(
            [
                ("id", "int8"),
                ("order_id", "int8"),
                ("method", "varchar"),
                ("amount", "numeric"),
                ("status", "varchar"),
            ],
            &[
                ["8201", "10482", "pix", "R$ 379,80", "approved"],
                ["8202", "10483", "credit_card", "R$ 129,90", "approved"],
                ["8203", "10484", "pix", "R$ 159,00", "approved"],
                ["8204", "10485", "credit_card", "R$ 349,00", "pending"],
            ],
        )
    } else if sql.contains("order_items") {
        fixture(
            [
                ("id", "int8"),
                ("order_id", "int8"),
                ("product_id", "int8"),
                ("quantity", "int8"),
                ("unit_price", "numeric"),
            ],
            &[
                ["23101", "10482", "101", "2", "R$ 189,90"],
                ["23102", "10483", "102", "1", "R$ 129,90"],
                ["23103", "10484", "103", "1", "R$ 159,00"],
                ["23104", "10485", "105", "1", "R$ 349,00"],
            ],
        )
    } else if sql.contains("reviews") {
        fixture(
            [
                ("id", "int8"),
                ("product_id", "int8"),
                ("rating", "int8"),
                ("comment", "text"),
            ],
            &[
                ["401", "101", "5", "Great sound quality."],
                ["402", "102", "4", "Beautiful and functional design."],
                ["403", "103", "5", "Excellent finish."],
                ["404", "105", "4", "Comfortable to work with."],
            ],
        )
    } else if sql.contains("orders") {
        fixture(
            [
                ("id", "int8"),
                ("customer_id", "int8"),
                ("total", "numeric"),
                ("status", "varchar"),
                ("created_at", "date"),
            ],
            &[
                ["10482", "1001", "R$ 379,80", "completed", "2026-09-30"],
                ["10483", "1003", "R$ 129,90", "completed", "2026-09-30"],
                ["10484", "1002", "R$ 159,00", "completed", "2026-09-29"],
                ["10485", "1005", "R$ 349,00", "pending", "2026-09-29"],
                ["10486", "1004", "R$ 79,90", "completed", "2026-09-28"],
                ["10487", "1006", "R$ 189,90", "completed", "2026-09-28"],
            ],
        )
    } else {
        ResultData::new(vec![ResultColumn::new("preview", "text")])
    }
}

/// Why the demo would refuse to run `sql`, or `None` when it accepts it. The
/// demo only simulates read queries; real connections never use this check.
pub fn preview_error(sql: &str) -> Option<&'static str> {
    let first = jykre::sql::tokens(sql, None).into_iter().next()?;
    let command = sql[first.0].to_ascii_uppercase();
    if !matches!(
        command.as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "DESCRIBE" | "VALUES"
    ) {
        Some(jykre::tr!(
            "The demo accepts only read queries (SELECT, WITH, EXPLAIN, SHOW, DESCRIBE or VALUES)."
        ))
    } else if command == "SELECT"
        && sql
            .trim_end_matches([';', ' ', '\n', '\r'])
            .trim()
            .eq_ignore_ascii_case("SELECT")
    {
        Some(jykre::tr!("SELECT needs an expression or column."))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::preview_error;

    #[test]
    fn the_demo_runs_only_read_queries_with_something_to_select() {
        assert!(preview_error("SELECT;").is_some());
        assert!(preview_error("DELETE FROM orders;").is_some());
        assert!(preview_error("SELECT 1;").is_none());
    }
}
