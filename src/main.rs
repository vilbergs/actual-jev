mod actual;
mod calibrate;
mod classify;
mod config;
mod filter;
mod import;
mod policy;
mod run;
mod tags;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use tracing::info;
use tracing_subscriber::EnvFilter;

use crate::actual::http::HttpStore;
use crate::actual::{ActualStore, ImportOptions, Snapshot};
use crate::classify::jev::JevClient;
use crate::classify::{Classifier, Subject, Taxonomy};
use crate::config::{Config, Secrets};
use crate::filter::Filter;

/// Categorize Actual Budget transactions with TypeSafe Jev.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML config file.
    #[arg(short, long, default_value = "jevcat.toml")]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the transactions a run would classify, without calling Jev.
    List {
        /// Include transactions tagged as missed on an earlier run.
        #[arg(long)]
        rerun_missed: bool,
    },
    /// Replay categorized history through Jev and report accuracy per confidence.
    Calibrate {
        /// How many of the most recent categorized transactions to replay.
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Precision the suggested threshold must reach.
        #[arg(long, default_value_t = 0.95)]
        target: f64,
    },
    /// Classify uncategorized transactions already in Actual. Dry run unless --apply is given.
    Run {
        /// Write categories and tags back to Actual.
        #[arg(long)]
        apply: bool,
        /// Retry transactions tagged as missed on an earlier run.
        #[arg(long)]
        rerun_missed: bool,
    },
    /// Import a bank CSV into an account, then categorize what the import added.
    Import {
        /// The CSV file the bank exported.
        csv: PathBuf,
        /// Actual account name or id. Overrides `import.account` in the config.
        #[arg(long)]
        account: Option<String>,
        /// Parse and classify but write nothing: no import, no categories, no tags.
        #[arg(long)]
        dry_run: bool,
        /// Import only; leave the new rows uncategorized.
        #[arg(long)]
        no_categorize: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let secrets = Secrets::from_env()?;

    let store = HttpStore::new(
        &config.actual.base_url,
        &config.actual.budget_sync_id,
        &secrets.actual_api_key,
        secrets.actual_encryption_password.as_deref(),
    )?;
    let since = since_date(config.actual.since_days);
    let snapshot = Snapshot::load(&store, &since).await?;
    let taxonomy = Taxonomy::from_groups(
        &snapshot.groups,
        &config.jev.descriptions,
        &config.jev.unsure_label,
    );
    anyhow::ensure!(
        taxonomy.options.len() <= 255,
        "Jev accepts at most 255 options; this budget has {} visible categories",
        taxonomy.options.len() - 1
    );

    match cli.command {
        Command::List { rerun_missed } => {
            let filter = Filter::new(&snapshot, &config.tags.missed, rerun_missed);
            let mut count = 0;
            for tx in snapshot
                .transactions
                .iter()
                .filter(|tx| filter.check(tx).is_ok())
            {
                let subject = Subject::from_transaction(tx, &snapshot);
                println!(
                    "{}  {:>10.2}  {:<24}  {}",
                    subject.date, subject.amount, subject.account, subject.payee
                );
                count += 1;
            }
            println!(
                "\n{count} eligible transactions, {} categories",
                taxonomy.options.len() - 1
            );
        }
        Command::Calibrate { limit, target } => {
            let classifier = classifier(&config, &secrets)?;
            let report = calibrate::calibrate(
                &snapshot,
                &taxonomy,
                classifier,
                &config.tags.missed,
                limit,
                config.jev.concurrency,
            )
            .await?;
            print!("{}", report.render(target));
        }
        Command::Run {
            apply,
            rerun_missed,
        } => {
            let classifier = classifier(&config, &secrets)?;
            let options = run::RunOptions {
                apply,
                rerun_missed,
                apply_threshold: config.policy.apply_threshold,
                concurrency: config.jev.concurrency,
                only_ids: None,
            };
            if !apply {
                println!("Dry run: nothing is written. Pass --apply to write.\n");
            }
            let summary = run::run(
                &store as &dyn ActualStore,
                &snapshot,
                &taxonomy,
                classifier,
                &config.tags,
                &options,
            )
            .await?;
            print!("{}", summary.render(apply));
        }
        Command::Import {
            csv,
            account,
            dry_run,
            no_categorize,
        } => {
            let import_config = config
                .import
                .as_ref()
                .context("add an [import] section to the config to use `jevcat import`")?;
            let account_name = account
                .as_deref()
                .or(import_config.account.as_deref())
                .context("pass --account or set import.account in the config")?;
            let account = snapshot.account(account_name).with_context(|| {
                let names: Vec<&str> = snapshot.accounts.iter().map(|a| a.name.as_str()).collect();
                format!("no account {account_name:?}; accounts are {names:?}")
            })?;

            let rows = import::read_csv(&csv, import_config)?;
            anyhow::ensure!(!rows.is_empty(), "{} holds no transactions", csv.display());
            let earliest = rows
                .iter()
                .map(|row| row.date.as_str())
                .min()
                .unwrap_or(&since);
            println!(
                "{} rows in {} for account {} ({} to {})",
                rows.len(),
                csv.display(),
                account.name,
                earliest,
                rows.iter().map(|row| row.date.as_str()).max().unwrap_or("")
            );
            if dry_run {
                println!("Dry run: nothing is written.\n");
            }

            let result = store
                .import_transactions(
                    &account.id,
                    &rows,
                    &ImportOptions {
                        dry_run,
                        payee_normalization: import_config.payee_normalization.clone(),
                    },
                )
                .await?;
            println!(
                "Import: {} added, {} matched existing",
                result.added.len(),
                result.updated.len()
            );
            if no_categorize || result.added.is_empty() {
                return Ok(());
            }
            if dry_run {
                println!("Categorization is skipped on a dry run: Actual did not create the rows, so there is nothing to classify.");
                return Ok(());
            }

            info!(account = %account.name, since = %earliest, "reloading imported transactions");
            let reload_since = earliest.min(since.as_str());
            let fresh = store.transactions(&account.id, reload_since).await?;
            let snapshot = Snapshot::new(
                snapshot.accounts.clone(),
                snapshot.groups.clone(),
                snapshot.payees.clone(),
                fresh,
            );
            let classifier = classifier(&config, &secrets)?;
            let options = run::RunOptions {
                apply: true,
                rerun_missed: false,
                apply_threshold: config.policy.apply_threshold,
                concurrency: config.jev.concurrency,
                only_ids: Some(result.added.iter().cloned().collect::<HashSet<_>>()),
            };
            let summary = run::run(
                &store as &dyn ActualStore,
                &snapshot,
                &taxonomy,
                classifier,
                &config.tags,
                &options,
            )
            .await?;
            print!("{}", summary.render(true));
        }
    }
    Ok(())
}

fn classifier(config: &Config, secrets: &Secrets) -> Result<Arc<dyn Classifier>> {
    Ok(Arc::new(JevClient::new(
        &config.jev.base_url,
        &secrets.typesafe_api_key,
        &config.jev.model,
        &config.jev.instructions,
        Duration::from_secs(config.jev.timeout_secs),
        config.jev.max_retries,
    )?))
}

fn since_date(days: u32) -> String {
    (chrono::Local::now().date_naive() - chrono::Duration::days(i64::from(days)))
        .format("%Y-%m-%d")
        .to_string()
}
