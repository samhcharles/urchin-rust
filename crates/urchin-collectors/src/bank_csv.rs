//! Bank CSV import connector.
//!
//! Drop any bank CSV files into ~/.local/share/urchin/imports/bank/.
//! Auto-detects common column layouts from Chase, BofA, and generic formats.
//!
//! Checkpoint: JSON { "<filename>": <rows_ingested>, ... }

use std::collections::HashMap;
use std::path::PathBuf;
use std::{fs, io};

use anyhow::Result;

use urchin_core::{
    event::{Actor, Event, EventKind, EventMeta},
    identity::Identity,
    journal::Journal,
};

use crate::state::state_dir;

pub struct BankCsvOpts {
    pub import_dir: PathBuf,
    pub checkpoint_path: PathBuf,
}

impl BankCsvOpts {
    pub fn defaults() -> Self {
        let import_dir = dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join(".local/share/urchin/imports/bank");
        Self {
            import_dir,
            checkpoint_path: state_dir().join("bank-csv.json"),
        }
    }
}

type Checkpoint = HashMap<String, usize>;

fn load_checkpoint(path: &PathBuf) -> Checkpoint {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_checkpoint(path: &PathBuf, ckpt: &Checkpoint) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, serde_json::to_string(ckpt)?)?;
    Ok(())
}

pub fn collect(journal: &Journal, identity: &Identity, opts: &BankCsvOpts) -> Result<usize> {
    if !opts.import_dir.exists() {
        return Ok(0);
    }

    let mut ckpt = load_checkpoint(&opts.checkpoint_path);
    let mut total = 0;

    let entries = fs::read_dir(&opts.import_dir)?;
    let mut csv_files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("csv"))
        .collect();
    csv_files.sort();

    for path in &csv_files {
        let filename = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        let rows_seen = *ckpt.get(&filename).unwrap_or(&0);

        let count = ingest_file(journal, identity, path, rows_seen)?;
        if count > 0 {
            *ckpt.entry(filename).or_insert(0) += count;
            total += count;
        }
    }

    if total > 0 {
        save_checkpoint(&opts.checkpoint_path, &ckpt)?;
        journal.flush()?;
    }
    Ok(total)
}

fn ingest_file(
    journal: &Journal,
    identity: &Identity,
    path: &PathBuf,
    skip_rows: usize,
) -> Result<usize> {
    let file = fs::File::open(path)?;
    let mut rdr = csv::Reader::from_reader(io::BufReader::new(file));

    let headers = rdr.headers()?.clone();
    let col = detect_columns(&headers);

    let mut count = 0;
    for (i, result) in rdr.records().enumerate() {
        if i < skip_rows {
            continue;
        }
        let record = result?;

        let date_str = col
            .date
            .and_then(|c| record.get(c))
            .unwrap_or("")
            .trim()
            .to_string();
        let amount_str = col
            .amount
            .and_then(|c| record.get(c))
            .unwrap_or("")
            .trim()
            .to_string();
        let merchant = col
            .merchant
            .and_then(|c| record.get(c))
            .unwrap_or("")
            .trim()
            .to_string();

        if merchant.is_empty() && amount_str.is_empty() {
            continue;
        }

        let amount: Option<f64> = amount_str
            .trim_matches(|c| c == '$' || c == ' ')
            .replace(',', "")
            .parse()
            .ok();

        let content = if merchant.is_empty() {
            format!("${}", amount_str)
        } else if let Some(a) = amount {
            format!("{}: ${:.2}", merchant, a.abs())
        } else {
            merchant.clone()
        };

        let ts = parse_date(&date_str);
        let mut event = Event::new("bank-csv", EventKind::Purchase, content);
        if let Some(t) = ts {
            event.timestamp = t;
        }
        event.meta = Some(EventMeta {
            amount,
            currency: Some("USD".to_string()),
            merchant: if merchant.is_empty() {
                None
            } else {
                Some(merchant)
            },
            ..Default::default()
        });
        event.actor = Some(Actor {
            account: Some(identity.account.clone()),
            device: Some(identity.device.clone()),
            workspace: None,
            node_id: Some(identity.node_id.clone()),
        });
        journal.append(&event)?;
        count += 1;
    }
    Ok(count)
}

struct Columns {
    date: Option<usize>,
    amount: Option<usize>,
    merchant: Option<usize>,
}

fn detect_columns(headers: &csv::StringRecord) -> Columns {
    let mut date = None;
    let mut amount = None;
    let mut merchant = None;

    for (i, h) in headers.iter().enumerate() {
        let h = h.trim().to_lowercase();
        if date.is_none()
            && matches!(
                h.as_str(),
                "date" | "transaction date" | "posted date" | "trans date"
            )
        {
            date = Some(i);
        }
        if amount.is_none()
            && matches!(
                h.as_str(),
                "amount" | "debit" | "credit" | "transaction amount"
            )
        {
            amount = Some(i);
        }
        if merchant.is_none()
            && matches!(
                h.as_str(),
                "description" | "merchant" | "payee" | "memo" | "name"
            )
        {
            merchant = Some(i);
        }
    }

    Columns {
        date,
        amount,
        merchant,
    }
}

fn parse_date(s: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    use chrono::NaiveDate;

    let s = s.trim();
    // Try common formats: MM/DD/YYYY, YYYY-MM-DD, MM-DD-YYYY
    let date = NaiveDate::parse_from_str(s, "%m/%d/%Y")
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d"))
        .or_else(|_| NaiveDate::parse_from_str(s, "%m-%d-%Y"))
        .ok()?;

    Some(date.and_hms_opt(12, 0, 0)?.and_utc())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use urchin_core::identity::Identity;
    use urchin_core::journal::Journal;

    fn setup(tmp: &TempDir) -> (Journal, Identity, BankCsvOpts) {
        let journal = Journal::new(tmp.path().join("journal.jsonl"));
        let identity = Identity::for_test("test", "test");
        let opts = BankCsvOpts {
            import_dir: tmp.path().join("bank"),
            checkpoint_path: tmp.path().join("ckpt.json"),
        };
        (journal, identity, opts)
    }

    #[test]
    fn no_import_dir_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn parses_chase_format() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::create_dir_all(&opts.import_dir).unwrap();
        fs::write(
            opts.import_dir.join("chase.csv"),
            "Transaction Date,Post Date,Description,Category,Type,Amount,Memo\n\
             01/15/2024,01/16/2024,BLUE BOTTLE COFFEE,Food,Sale,-4.50,\n\
             01/16/2024,01/17/2024,WHOLE FOODS,Groceries,Sale,-62.40,\n",
        )
        .unwrap();

        let n = collect(&j, &id, &opts).unwrap();
        assert_eq!(n, 2);

        let events = j.read_all().unwrap();
        assert_eq!(events[0].kind, EventKind::Purchase);
        let meta = events[0].meta.as_ref().unwrap();
        assert!(meta.amount.is_some());
        assert_eq!(meta.currency.as_deref(), Some("USD"));
    }

    #[test]
    fn checkpoint_skips_seen_rows() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::create_dir_all(&opts.import_dir).unwrap();
        fs::write(
            opts.import_dir.join("test.csv"),
            "Transaction Date,Description,Amount\n\
             01/15/2024,Coffee,-4.50\n\
             01/16/2024,Lunch,-12.00\n",
        )
        .unwrap();

        assert_eq!(collect(&j, &id, &opts).unwrap(), 2);
        assert_eq!(collect(&j, &id, &opts).unwrap(), 0);
    }

    #[test]
    fn generic_columns_detected() {
        let tmp = TempDir::new().unwrap();
        let (j, id, opts) = setup(&tmp);
        fs::create_dir_all(&opts.import_dir).unwrap();
        fs::write(
            opts.import_dir.join("generic.csv"),
            "Date,Payee,Amount\n\
             2024-01-15,Amazon,39.99\n",
        )
        .unwrap();

        assert_eq!(collect(&j, &id, &opts).unwrap(), 1);
        let events = j.read_all().unwrap();
        assert!(events[0].content.contains("Amazon"));
    }
}
