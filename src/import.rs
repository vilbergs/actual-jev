//! Reads a bank CSV export into the transactions Actual's import endpoint accepts.
//!
//! Banks disagree on everything: delimiter, header names, date format, whether the
//! decimal mark is a comma, whether there is a preamble. All of that is config; the
//! code only has to be forgiving about amounts and stable about `imported_id`.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::NaiveDate;
use serde::Deserialize;

use crate::actual::NewTransaction;

#[derive(Debug, Clone, Deserialize)]
pub struct ImportConfig {
    /// Name or id of the Actual account the CSV belongs to. `--account` overrides it.
    #[serde(default)]
    pub account: Option<String>,
    /// Column that holds the booking date. A header name, or a 0-based index without a header.
    pub date_column: String,
    /// strftime pattern for the date column.
    #[serde(default = "default_date_format")]
    pub date_format: String,
    pub amount_column: String,
    pub payee_column: String,
    #[serde(default)]
    pub notes_column: Option<String>,
    /// A column the bank guarantees unique per transaction. Without one, a hash of the
    /// row's date, amount, payee and notes is used.
    #[serde(default)]
    pub id_column: Option<String>,
    #[serde(default = "default_delimiter")]
    pub delimiter: char,
    #[serde(default = "default_true")]
    pub has_header: bool,
    /// Lines to drop before the table starts, for exports with a preamble.
    #[serde(default)]
    pub skip_rows: usize,
    /// How Actual normalizes payee names on import: `title-case` or `original`.
    #[serde(default = "default_payee_normalization")]
    pub payee_normalization: String,
}

fn default_date_format() -> String {
    "%Y-%m-%d".to_string()
}
fn default_delimiter() -> char {
    ','
}
fn default_true() -> bool {
    true
}
fn default_payee_normalization() -> String {
    "title-case".to_string()
}

#[derive(Debug, Clone, Copy)]
enum Column {
    Name(usize),
    Index(usize),
}

struct Columns {
    date: Column,
    amount: Column,
    payee: Column,
    notes: Option<Column>,
    id: Option<Column>,
}

pub fn read_csv(path: &Path, config: &ImportConfig) -> Result<Vec<NewTransaction>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse_csv(&text, config).with_context(|| format!("parsing {}", path.display()))
}

pub fn parse_csv(text: &str, config: &ImportConfig) -> Result<Vec<NewTransaction>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let body: String = text
        .lines()
        .skip(config.skip_rows)
        .collect::<Vec<_>>()
        .join("\n");

    let mut reader = csv::ReaderBuilder::new()
        .delimiter(config.delimiter as u8)
        .has_headers(config.has_header)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(body.as_bytes());

    let headers: Vec<String> = if config.has_header {
        reader.headers()?.iter().map(str::to_string).collect()
    } else {
        Vec::new()
    };
    let columns = resolve_columns(config, &headers)?;

    let mut out = Vec::new();
    for (line, record) in reader.records().enumerate() {
        let record = record?;
        if record.iter().all(|field| field.is_empty()) {
            continue;
        }
        let row = line + 1;
        let date = field(&record, columns.date, &headers);
        let date = NaiveDate::parse_from_str(date, &config.date_format)
            .with_context(|| {
                format!(
                    "row {row}: date {date:?} does not match {}",
                    config.date_format
                )
            })?
            .format("%Y-%m-%d")
            .to_string();
        let amount_text = field(&record, columns.amount, &headers);
        let amount = parse_amount(amount_text)
            .with_context(|| format!("row {row}: cannot read amount {amount_text:?}"))?;
        let payee = field(&record, columns.payee, &headers).to_string();
        let notes = columns
            .notes
            .map(|column| field(&record, column, &headers).to_string())
            .filter(|notes| !notes.is_empty());
        let imported_id = match columns.id {
            Some(column) => {
                let id = field(&record, column, &headers);
                if id.is_empty() {
                    row_hash(&date, amount, &payee, notes.as_deref())
                } else {
                    id.to_string()
                }
            }
            None => row_hash(&date, amount, &payee, notes.as_deref()),
        };
        out.push(NewTransaction {
            date,
            amount,
            payee_name: payee.clone(),
            imported_payee: payee,
            notes,
            imported_id,
        });
    }
    Ok(out)
}

fn resolve_columns(config: &ImportConfig, headers: &[String]) -> Result<Columns> {
    let resolve = |spec: &str| -> Result<Column> {
        if config.has_header {
            let index = headers
                .iter()
                .position(|header| header.eq_ignore_ascii_case(spec))
                .with_context(|| format!("column {spec:?} not found; headers are {headers:?}"))?;
            Ok(Column::Name(index))
        } else {
            let index: usize = spec.parse().with_context(|| {
                format!("column {spec:?} must be a 0-based index when has_header = false")
            })?;
            Ok(Column::Index(index))
        }
    };
    Ok(Columns {
        date: resolve(&config.date_column)?,
        amount: resolve(&config.amount_column)?,
        payee: resolve(&config.payee_column)?,
        notes: config.notes_column.as_deref().map(resolve).transpose()?,
        id: config.id_column.as_deref().map(resolve).transpose()?,
    })
}

fn field<'a>(record: &'a csv::StringRecord, column: Column, _headers: &[String]) -> &'a str {
    let index = match column {
        Column::Name(index) | Column::Index(index) => index,
    };
    record.get(index).unwrap_or("")
}

/// Reads `-1 234,56`, `-1.234,56`, `1,234.56`, `1234.56`, `−12` and `12 kr` as minor units.
pub fn parse_amount(text: &str) -> Result<i64> {
    let cleaned: String = text
        .chars()
        .filter(|c| c.is_ascii_digit() || matches!(c, '-' | '+' | ',' | '.' | '−'))
        .map(|c| if c == '−' { '-' } else { c })
        .collect();
    anyhow::ensure!(!cleaned.is_empty(), "no digits in {text:?}");

    let negative = cleaned.starts_with('-');
    let digits = cleaned.trim_start_matches(['-', '+']);

    let last_comma = digits.rfind(',');
    let last_dot = digits.rfind('.');
    let decimal_at = match (last_comma, last_dot) {
        (Some(comma), Some(dot)) => Some(comma.max(dot)),
        (Some(comma), None) => {
            // A lone comma is a decimal mark unless it groups thousands (exactly 3 digits follow
            // and there is more than one comma, as in 1,234,567).
            let after = digits.len() - comma - 1;
            if after == 3 && digits.matches(',').count() > 1 {
                None
            } else {
                Some(comma)
            }
        }
        (None, Some(dot)) => {
            let after = digits.len() - dot - 1;
            if after == 3 && digits.matches('.').count() > 1 {
                None
            } else {
                Some(dot)
            }
        }
        (None, None) => None,
    };

    let (whole, fraction) = match decimal_at {
        Some(at) => (&digits[..at], &digits[at + 1..]),
        None => (digits, ""),
    };
    let whole: String = whole.chars().filter(char::is_ascii_digit).collect();
    let fraction: String = fraction.chars().filter(char::is_ascii_digit).collect();
    anyhow::ensure!(fraction.len() <= 2, "more than two decimals in {text:?}");

    let major: i64 = if whole.is_empty() { 0 } else { whole.parse()? };
    let minor: i64 = if fraction.is_empty() {
        0
    } else {
        format!("{fraction:0<2}").parse()?
    };
    let value = major * 100 + minor;
    Ok(if negative { -value } else { value })
}

/// FNV-1a over the row's identifying fields. Stable across runs and Rust versions, which
/// `DefaultHasher` is not.
fn row_hash(date: &str, amount: i64, payee: &str, notes: Option<&str>) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in format!("{date}|{amount}|{payee}|{}", notes.unwrap_or("")).bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("jevcat-{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ImportConfig {
        ImportConfig {
            account: None,
            date_column: "Bokföringsdag".into(),
            date_format: "%Y-%m-%d".into(),
            amount_column: "Belopp".into(),
            payee_column: "Rubrik".into(),
            notes_column: Some("Meddelande".into()),
            id_column: None,
            delimiter: ';',
            has_header: true,
            skip_rows: 0,
            payee_normalization: "title-case".into(),
        }
    }

    #[test]
    fn amounts_in_common_bank_formats() {
        assert_eq!(parse_amount("-847,50").unwrap(), -84750);
        assert_eq!(parse_amount("-1 234,56").unwrap(), -123456);
        assert_eq!(parse_amount("-1.234,56").unwrap(), -123456);
        assert_eq!(parse_amount("1,234.56").unwrap(), 123456);
        assert_eq!(parse_amount("1234.5").unwrap(), 123450);
        assert_eq!(parse_amount("12").unwrap(), 1200);
        assert_eq!(parse_amount("−12 kr").unwrap(), -1200);
        assert_eq!(parse_amount("1,234,567").unwrap(), 123456700);
        assert_eq!(parse_amount("+99,00 SEK").unwrap(), 9900);
        assert!(parse_amount("n/a").is_err());
    }

    #[test]
    fn parses_a_swedish_export_with_bom_and_blank_lines() {
        let text = "\u{feff}Bokföringsdag;Belopp;Rubrik;Meddelande\n\
                    2026-10-08;-847,50;ICA KVANTUM LIDINGÖ;Kortköp\n\
                    ;;;\n\
                    2026-10-07;25 000,00;LÖN;\n";
        let rows = parse_csv(text, &config()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].date, "2026-10-08");
        assert_eq!(rows[0].amount, -84750);
        assert_eq!(rows[0].payee_name, "ICA KVANTUM LIDINGÖ");
        assert_eq!(rows[0].notes.as_deref(), Some("Kortköp"));
        assert!(rows[0].imported_id.starts_with("jevcat-"));
        assert_eq!(rows[1].amount, 2_500_000);
        assert_eq!(rows[1].notes, None);
        assert_ne!(rows[0].imported_id, rows[1].imported_id);
    }

    #[test]
    fn imported_id_is_stable_and_prefers_the_bank_id() {
        let mut config = config();
        let text = "Bokföringsdag;Belopp;Rubrik;Meddelande\n2026-10-08;-1,00;A;x\n";
        let first = parse_csv(text, &config).unwrap();
        let second = parse_csv(text, &config).unwrap();
        assert_eq!(first[0].imported_id, second[0].imported_id);

        config.id_column = Some("Meddelande".into());
        let with_id = parse_csv(text, &config).unwrap();
        assert_eq!(with_id[0].imported_id, "x");
    }

    #[test]
    fn preamble_and_other_date_formats() {
        let mut config = config();
        config.skip_rows = 2;
        config.date_format = "%d/%m/%Y".into();
        config.delimiter = ',';
        config.date_column = "Date".into();
        config.amount_column = "Amount".into();
        config.payee_column = "Description".into();
        config.notes_column = None;
        let text = "Account statement\nGenerated 2026-10-10\nDate,Amount,Description\n08/10/2026,-10.00,Coffee\n";
        let rows = parse_csv(text, &config).unwrap();
        assert_eq!(rows[0].date, "2026-10-08");
        assert_eq!(rows[0].amount, -1000);
    }

    #[test]
    fn missing_column_is_a_clear_error() {
        let mut config = config();
        config.payee_column = "Nope".into();
        let error = parse_csv("Bokföringsdag;Belopp;Rubrik\n", &config).unwrap_err();
        assert!(error.to_string().contains("Nope"));
    }
}
