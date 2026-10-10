//! Turns a classifier decision into the one thing the run does to the transaction.

use crate::classify::{Decision, Taxonomy};

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Confident enough: write the category and the applied tag.
    Apply {
        category_id: String,
        label: String,
        confidence: f64,
    },
    /// A real category but below the threshold: leave uncategorized, record the guess.
    Flag { label: String, confidence: f64 },
    /// The classifier picked the escape hatch or an unknown label: mark as missed.
    Skip { label: String, confidence: f64 },
}

pub fn decide(decision: &Decision, taxonomy: &Taxonomy, apply_threshold: f64) -> Action {
    let category_id = taxonomy
        .option(&decision.label)
        .and_then(|option| option.category_id.clone());
    match category_id {
        Some(category_id) if decision.confidence >= apply_threshold => Action::Apply {
            category_id,
            label: decision.label.clone(),
            confidence: decision.confidence,
        },
        Some(_) => Action::Flag {
            label: decision.label.clone(),
            confidence: decision.confidence,
        },
        None => Action::Skip {
            label: decision.label.clone(),
            confidence: decision.confidence,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::TaxonomyOption;
    use std::collections::BTreeMap;

    fn taxonomy() -> Taxonomy {
        Taxonomy::new(vec![
            TaxonomyOption {
                label: "Groceries".into(),
                category_id: Some("c1".into()),
                description: None,
            },
            TaxonomyOption {
                label: "Unsure".into(),
                category_id: None,
                description: None,
            },
        ])
    }

    fn decision(label: &str, confidence: f64) -> Decision {
        Decision {
            label: label.into(),
            confidence,
            probabilities: BTreeMap::new(),
            input_tokens: 0,
        }
    }

    #[test]
    fn applies_at_or_above_threshold() {
        assert_eq!(
            decide(&decision("Groceries", 0.85), &taxonomy(), 0.85),
            Action::Apply {
                category_id: "c1".into(),
                label: "Groceries".into(),
                confidence: 0.85
            }
        );
    }

    #[test]
    fn flags_below_threshold() {
        assert_eq!(
            decide(&decision("Groceries", 0.6), &taxonomy(), 0.85),
            Action::Flag {
                label: "Groceries".into(),
                confidence: 0.6
            }
        );
    }

    #[test]
    fn skips_unsure_and_unknown_labels_whatever_the_confidence() {
        assert!(matches!(
            decide(&decision("Unsure", 0.99), &taxonomy(), 0.85),
            Action::Skip { .. }
        ));
        assert!(matches!(
            decide(&decision("Not a label", 0.99), &taxonomy(), 0.85),
            Action::Skip { .. }
        ));
    }
}
