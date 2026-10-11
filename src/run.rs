//! One pass over the budget: load, filter, classify, decide, write, learn.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use tracing::{info, warn};

use crate::actual::{ActualStore, Snapshot, Transaction, TransactionPatch};
use crate::classify::{Classifier, Subject, Taxonomy};
use crate::config::TagConfig;
use crate::filter::{Filter, Skip};
use crate::learn::{self, Applied};
use crate::policy::{self, Action};
use crate::tags;

pub struct RunOptions {
    pub apply: bool,
    pub rerun_missed: bool,
    pub apply_threshold: f64,
    pub concurrency: usize,
    /// When set, only these transaction ids are considered (the rows an import just added).
    pub only_ids: Option<HashSet<String>>,
    /// Create an Actual rule for a payee once this many confident applies agree. 0 disables.
    pub learn_after: usize,
}

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub considered: usize,
    pub skipped: BTreeMap<String, usize>,
    pub applied: usize,
    pub flagged: usize,
    pub missed: usize,
    pub failed: usize,
    pub rules_created: usize,
    pub input_tokens: u64,
}

/// The tag prefix shared by all three tags, used when stripping old ones from a note.
fn tag_prefix(tags: &TagConfig) -> String {
    let candidates = [&tags.applied, &tags.flagged, &tags.missed];
    let shortest = candidates.iter().min_by_key(|tag| tag.len()).unwrap();
    let prefix: String = shortest
        .chars()
        .enumerate()
        .take_while(|(i, c)| candidates.iter().all(|tag| tag.chars().nth(*i) == Some(*c)))
        .map(|(_, c)| c)
        .collect();
    if prefix.len() > 1 {
        prefix
    } else {
        shortest.to_string()
    }
}

pub fn patch_for(action: &Action, tx: &Transaction, tags: &TagConfig) -> TransactionPatch {
    let notes = tx.notes.as_deref().unwrap_or("");
    let prefix = tag_prefix(tags);
    match action {
        Action::Apply { category_id, .. } => TransactionPatch {
            category: Some(category_id.clone()),
            notes: Some(tags::with_tag(notes, &prefix, &tags.applied, None)),
        },
        Action::Flag { label, confidence } => TransactionPatch {
            category: None,
            notes: Some(tags::with_tag(
                notes,
                &prefix,
                &tags.flagged,
                Some(&format!("{label} {confidence:.2}")),
            )),
        },
        Action::Skip { .. } => TransactionPatch {
            category: None,
            notes: Some(tags::with_tag(notes, &prefix, &tags.missed, None)),
        },
    }
}

pub async fn run(
    store: &dyn ActualStore,
    snapshot: &Snapshot,
    taxonomy: &Taxonomy,
    classifier: Arc<dyn Classifier>,
    tags: &TagConfig,
    options: &RunOptions,
) -> Result<Summary> {
    let mut summary = Summary::default();
    let filter = Filter::new(snapshot, &tags.missed, options.rerun_missed);

    let mut eligible: Vec<&Transaction> = Vec::new();
    for tx in &snapshot.transactions {
        if let Some(only) = &options.only_ids {
            if !only.contains(&tx.id) {
                continue;
            }
        }
        summary.considered += 1;
        match filter.check(tx) {
            Ok(()) => eligible.push(tx),
            Err(reason) => *summary.skipped.entry(skip_key(reason)).or_default() += 1,
        }
    }
    info!(
        eligible = eligible.len(),
        considered = summary.considered,
        "transactions to classify"
    );

    let outcomes: Vec<(&Transaction, Result<(Action, u64)>)> = stream::iter(eligible)
        .map(|tx| {
            let classifier = Arc::clone(&classifier);
            async move {
                let subject = Subject::from_transaction(tx, snapshot);
                let result = classifier
                    .classify(&subject, taxonomy)
                    .await
                    .map(|decision| {
                        let action = policy::decide(&decision, taxonomy, options.apply_threshold);
                        (action, decision.input_tokens)
                    });
                (tx, result)
            }
        })
        .buffer_unordered(options.concurrency.max(1))
        .collect()
        .await;

    let mode = if options.apply { "apply" } else { "dry-run" };
    let mut applied_now: Vec<Applied> = Vec::new();

    for (tx, outcome) in outcomes {
        let (action, tokens) = match outcome {
            Ok(value) => value,
            Err(error) => {
                summary.failed += 1;
                warn!(id = %tx.id, %error, "classification failed, leaving untouched");
                continue;
            }
        };
        summary.input_tokens += tokens;
        let patch = patch_for(&action, tx, tags);
        let subject = Subject::from_transaction(tx, snapshot);
        let payee = truncate(&subject.payee, 32);
        match &action {
            Action::Apply {
                category_id,
                label,
                confidence,
            } => {
                summary.applied += 1;
                println!(
                    "{mode}  APPLY  {}  {:>10.2}  {payee:<32}  -> {label} ({confidence:.2})",
                    tx.date, subject.amount
                );
                if let Some(payee_id) = tx.payee.clone() {
                    applied_now.push(Applied {
                        payee_id,
                        category_id: category_id.clone(),
                    });
                }
            }
            Action::Flag { label, confidence } => {
                summary.flagged += 1;
                println!(
                    "{mode}  FLAG   {}  {:>10.2}  {payee:<32}  ?  {label} ({confidence:.2})",
                    tx.date, subject.amount
                );
            }
            Action::Skip { label, confidence } => {
                summary.missed += 1;
                println!(
                    "{mode}  MISS   {}  {:>10.2}  {payee:<32}  {label} ({confidence:.2})",
                    tx.date, subject.amount
                );
            }
        }
        if options.apply {
            store.update_transaction(&tx.id, &patch).await?;
        }
    }

    if options.learn_after > 0 {
        summary.rules_created = learn_rules(
            store,
            snapshot,
            taxonomy,
            &applied_now,
            &tags.applied,
            options.learn_after,
            options.apply,
        )
        .await?;
    }

    Ok(summary)
}

/// Proposes and, when applying, creates payee rules. Returns how many were created or
/// would be created.
pub async fn learn_rules(
    store: &dyn ActualStore,
    snapshot: &Snapshot,
    taxonomy: &Taxonomy,
    applied_now: &[Applied],
    applied_tag: &str,
    learn_after: usize,
    apply: bool,
) -> Result<usize> {
    let rules = store.rules().await?;
    let candidates = learn::candidates(snapshot, applied_now, &rules, applied_tag, learn_after);
    let mode = if apply { "apply" } else { "dry-run" };
    for candidate in &candidates {
        let label = taxonomy
            .label_for_category(&candidate.category_id)
            .unwrap_or(candidate.category_id.as_str());
        println!(
            "{mode}  RULE   {:<32}  -> {label} (seen {} times)",
            truncate(&candidate.payee_name, 32),
            candidate.count
        );
        if apply {
            store.create_rule(&candidate.rule()).await?;
        }
    }
    Ok(candidates.len())
}

fn skip_key(reason: Skip) -> String {
    reason.to_string()
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut out: String = text.chars().take(max - 1).collect();
        out.push('…');
        out
    }
}

impl Summary {
    pub fn render(&self, apply: bool) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "\n{} transactions considered, {} eligible\n",
            self.considered,
            self.applied + self.flagged + self.missed + self.failed
        ));
        for (reason, count) in &self.skipped {
            out.push_str(&format!("  skipped {count:>5}  {reason}\n"));
        }
        let verb = if apply { "written" } else { "would write" };
        out.push_str(&format!(
            "{verb}: {} applied, {} flagged, {} missed, {} failed, {} rules\n",
            self.applied, self.flagged, self.missed, self.failed, self.rules_created
        ));
        out.push_str(&format!("Jev input tokens: {}\n", self.input_tokens));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tx(notes: Option<&str>) -> Transaction {
        Transaction {
            id: "t1".into(),
            account: "a1".into(),
            date: "2026-10-01".into(),
            amount: -1000,
            payee: Some("p1".into()),
            imported_payee: None,
            category: None,
            notes: notes.map(str::to_string),
            transfer_id: None,
            is_parent: false,
            is_child: false,
            starting_balance_flag: false,
        }
    }

    #[test]
    fn prefix_is_the_common_start_of_the_tags() {
        assert_eq!(tag_prefix(&TagConfig::default()), "#jev");
        let custom = TagConfig {
            applied: "#ai".into(),
            flagged: "#ai-flag".into(),
            missed: "#ai-miss".into(),
        };
        assert_eq!(tag_prefix(&custom), "#ai");
    }

    #[test]
    fn patches_carry_category_and_tags() {
        let tags = TagConfig::default();
        let apply = Action::Apply {
            category_id: "c1".into(),
            label: "Groceries".into(),
            confidence: 0.9,
        };
        assert_eq!(
            patch_for(&apply, &tx(Some("weekly")), &tags),
            TransactionPatch {
                category: Some("c1".into()),
                notes: Some("weekly #jev".into()),
            }
        );

        let flag = Action::Flag {
            label: "Groceries".into(),
            confidence: 0.7,
        };
        assert_eq!(
            patch_for(&flag, &tx(Some("weekly #jev-miss")), &tags),
            TransactionPatch {
                category: None,
                notes: Some("weekly #jev-flag [jev: Groceries 0.70]".into()),
            }
        );

        let skip = Action::Skip {
            label: "Unsure".into(),
            confidence: 0.4,
        };
        assert_eq!(
            patch_for(&skip, &tx(None), &tags),
            TransactionPatch {
                category: None,
                notes: Some("#jev-miss".into()),
            }
        );
    }
}
