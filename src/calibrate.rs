//! Replays already-categorized transactions through the classifier and reports how
//! accuracy varies with confidence, so the apply threshold is measured rather than guessed.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use futures::stream::{self, StreamExt};
use tracing::info;

use crate::actual::{Snapshot, Transaction};
use crate::classify::{Classifier, Subject, Taxonomy};
use crate::filter::Filter;

#[derive(Debug, Clone, PartialEq)]
pub struct Sample {
    pub transaction_id: String,
    pub payee: String,
    pub truth: String,
    pub guess: String,
    pub confidence: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bucket {
    pub lower: f64,
    pub total: usize,
    pub correct: usize,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub samples: Vec<Sample>,
    pub input_tokens: u64,
}

impl Report {
    /// Ten buckets of width 0.1, lowest first. A sample lands in the bucket that
    /// contains its confidence.
    pub fn buckets(&self) -> Vec<Bucket> {
        let mut buckets: Vec<Bucket> = (0..10)
            .map(|i| Bucket {
                lower: i as f64 / 10.0,
                total: 0,
                correct: 0,
            })
            .collect();
        for sample in &self.samples {
            let index = ((sample.confidence * 10.0).floor() as usize).min(9);
            buckets[index].total += 1;
            if sample.guess == sample.truth {
                buckets[index].correct += 1;
            }
        }
        buckets
    }

    /// Precision of everything at or above `threshold`, and how many samples that is.
    pub fn precision_at(&self, threshold: f64) -> (f64, usize) {
        let above: Vec<&Sample> = self
            .samples
            .iter()
            .filter(|sample| sample.confidence >= threshold)
            .collect();
        if above.is_empty() {
            return (0.0, 0);
        }
        let correct = above
            .iter()
            .filter(|sample| sample.guess == sample.truth)
            .count();
        (correct as f64 / above.len() as f64, above.len())
    }

    /// The lowest bucket boundary whose cumulative precision reaches `target`, if any.
    pub fn suggested_threshold(&self, target: f64) -> Option<f64> {
        (0..=10).map(|i| i as f64 / 10.0).find(|threshold| {
            let (precision, count) = self.precision_at(*threshold);
            count > 0 && precision >= target
        })
    }

    /// The most frequent wrong (truth, guess) pairs.
    pub fn confusions(&self, limit: usize) -> Vec<((String, String), usize)> {
        let mut counts: BTreeMap<(String, String), usize> = BTreeMap::new();
        for sample in self.samples.iter().filter(|s| s.guess != s.truth) {
            *counts
                .entry((sample.truth.clone(), sample.guess.clone()))
                .or_default() += 1;
        }
        let mut pairs: Vec<_> = counts.into_iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        pairs.truncate(limit);
        pairs
    }

    pub fn render(&self, target: f64) -> String {
        let mut out = String::new();
        out.push_str(&format!("{} samples\n\n", self.samples.len()));
        out.push_str("confidence   n   correct   accuracy\n");
        for bucket in self.buckets().iter().filter(|b| b.total > 0) {
            out.push_str(&format!(
                "{:.1}-{:.1}   {:>4}   {:>7}   {:>7.1}%\n",
                bucket.lower,
                bucket.lower + 0.1,
                bucket.total,
                bucket.correct,
                100.0 * bucket.correct as f64 / bucket.total as f64
            ));
        }
        out.push_str("\nthreshold   precision   coverage\n");
        for i in (5..=9).rev() {
            let threshold = i as f64 / 10.0;
            let (precision, count) = self.precision_at(threshold);
            out.push_str(&format!(
                ">= {:.1}      {:>6.1}%   {:>4} of {}\n",
                threshold,
                100.0 * precision,
                count,
                self.samples.len()
            ));
        }
        match self.suggested_threshold(target) {
            Some(threshold) => out.push_str(&format!(
                "\nSuggested apply_threshold for {:.0}% precision: {threshold:.1}\n",
                100.0 * target
            )),
            None => out.push_str(&format!(
                "\nNo threshold reaches {:.0}% precision on this sample. Improve the category descriptions and rerun.\n",
                100.0 * target
            )),
        }
        let confusions = self.confusions(8);
        if !confusions.is_empty() {
            out.push_str("\nMost common mistakes (truth -> guess):\n");
            for ((truth, guess), count) in confusions {
                out.push_str(&format!("{count:>4}   {truth} -> {guess}\n"));
            }
        }
        out.push_str(&format!("\nJev input tokens: {}\n", self.input_tokens));
        out
    }
}

/// Picks the `limit` most recent categorized transactions that the classifier would
/// have been allowed to touch, and classifies them.
pub async fn calibrate(
    snapshot: &Snapshot,
    taxonomy: &Taxonomy,
    classifier: Arc<dyn Classifier>,
    missed_tag: &str,
    limit: usize,
    concurrency: usize,
) -> Result<Report> {
    let filter = Filter::new(snapshot, missed_tag, true);
    let mut candidates: Vec<&Transaction> = snapshot
        .transactions
        .iter()
        .filter(|tx| filter.check_shape(tx).is_ok())
        .filter(|tx| {
            tx.category
                .as_deref()
                .is_some_and(|id| taxonomy.label_for_category(id).is_some())
        })
        .collect();
    candidates.sort_by(|a, b| b.date.cmp(&a.date));
    candidates.truncate(limit);
    info!(count = candidates.len(), "calibrating");

    let results: Vec<Result<(Sample, u64)>> = stream::iter(candidates)
        .map(|tx| {
            let classifier = Arc::clone(&classifier);
            async move {
                let subject = Subject::from_transaction(tx, snapshot);
                let decision = classifier.classify(&subject, taxonomy).await?;
                let truth = taxonomy
                    .label_for_category(tx.category.as_deref().unwrap_or_default())
                    .unwrap_or_default()
                    .to_string();
                Ok((
                    Sample {
                        transaction_id: tx.id.clone(),
                        payee: subject.payee,
                        truth,
                        guess: decision.label,
                        confidence: decision.confidence,
                    },
                    decision.input_tokens,
                ))
            }
        })
        .buffer_unordered(concurrency.max(1))
        .collect()
        .await;

    let mut report = Report::default();
    for result in results {
        let (sample, tokens) = result?;
        report.samples.push(sample);
        report.input_tokens += tokens;
    }
    report
        .samples
        .sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(truth: &str, guess: &str, confidence: f64) -> Sample {
        Sample {
            transaction_id: String::new(),
            payee: String::new(),
            truth: truth.into(),
            guess: guess.into(),
            confidence,
        }
    }

    fn report() -> Report {
        Report {
            samples: vec![
                sample("A", "A", 0.95),
                sample("A", "A", 0.92),
                sample("B", "B", 0.88),
                sample("B", "A", 0.81),
                sample("C", "A", 0.55),
                sample("C", "C", 1.0),
            ],
            input_tokens: 10,
        }
    }

    #[test]
    fn buckets_and_precision() {
        let report = report();
        let buckets = report.buckets();
        assert_eq!(buckets[9].total, 3);
        assert_eq!(buckets[9].correct, 3);
        assert_eq!(buckets[8].total, 2);
        assert_eq!(buckets[8].correct, 1);
        assert_eq!(buckets[5].total, 1);
        assert_eq!(report.precision_at(0.9), (1.0, 3));
        let (precision, count) = report.precision_at(0.8);
        assert_eq!(count, 5);
        assert!((precision - 0.8).abs() < 1e-9);
    }

    #[test]
    fn suggested_threshold_is_the_lowest_that_reaches_target() {
        let report = report();
        assert_eq!(report.suggested_threshold(0.95), Some(0.9));
        assert_eq!(report.suggested_threshold(0.8), Some(0.6));
        assert_eq!(report.suggested_threshold(1.01), None);
    }

    #[test]
    fn confusions_are_counted() {
        let report = report();
        let confusions = report.confusions(5);
        assert_eq!(confusions[0], (("B".into(), "A".into()), 1));
        assert_eq!(confusions.len(), 2);
    }
}
