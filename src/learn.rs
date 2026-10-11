//! Turns repeated confident guesses into Actual rules, so a payee Jev has placed a few
//! times is categorized by Actual itself from then on and never reaches Jev again.
//!
//! Evidence for a payee is every transaction that carries the applied tag (an earlier
//! run wrote it) plus this run's applies. A rule is proposed when the evidence agrees on
//! one category, reaches `min_count`, no human-categorized transaction of that payee
//! disagrees, and no rule already targets the payee.

use std::collections::{BTreeMap, HashSet};

use crate::actual::{Rule, RuleClause, Snapshot};
use crate::tags;

#[derive(Debug, Clone, PartialEq)]
pub struct Applied {
    pub payee_id: String,
    pub category_id: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub payee_id: String,
    pub payee_name: String,
    pub category_id: String,
    pub count: usize,
}

impl Candidate {
    pub fn rule(&self) -> Rule {
        Rule {
            id: None,
            stage: None,
            conditions_op: "and".to_string(),
            conditions: vec![RuleClause {
                field: "payee".to_string(),
                op: "is".to_string(),
                value: serde_json::Value::String(self.payee_id.clone()),
                kind: Some("id".to_string()),
            }],
            actions: vec![RuleClause {
                field: "category".to_string(),
                op: "set".to_string(),
                value: serde_json::Value::String(self.category_id.clone()),
                kind: Some("id".to_string()),
            }],
        }
    }
}

pub fn candidates(
    snapshot: &Snapshot,
    applied_now: &[Applied],
    rules: &[Rule],
    applied_tag: &str,
    min_count: usize,
) -> Vec<Candidate> {
    let covered: HashSet<&str> = rules.iter().filter_map(payee_of_rule).collect();

    // payee -> (category -> jev count), plus the set of categories a person chose.
    let mut jev: BTreeMap<&str, BTreeMap<&str, usize>> = BTreeMap::new();
    let mut human: BTreeMap<&str, HashSet<&str>> = BTreeMap::new();

    for tx in &snapshot.transactions {
        let (Some(payee), Some(category)) = (tx.payee.as_deref(), tx.category.as_deref()) else {
            continue;
        };
        if tx.is_parent || tx.transfer_id.as_deref().is_some_and(|id| !id.is_empty()) {
            continue;
        }
        let notes = tx.notes.as_deref().unwrap_or("");
        if tags::has_tag(notes, applied_tag) {
            *jev.entry(payee).or_default().entry(category).or_default() += 1;
        } else {
            human.entry(payee).or_default().insert(category);
        }
    }
    for applied in applied_now {
        *jev.entry(&applied.payee_id)
            .or_default()
            .entry(&applied.category_id)
            .or_default() += 1;
    }

    jev.into_iter()
        .filter(|(payee, _)| !covered.contains(payee))
        .filter_map(|(payee, by_category)| {
            if by_category.len() != 1 {
                return None;
            }
            let (category, count) = by_category.into_iter().next()?;
            if count < min_count {
                return None;
            }
            if human
                .get(payee)
                .is_some_and(|chosen| chosen.iter().any(|c| *c != category))
            {
                return None;
            }
            Some(Candidate {
                payee_id: payee.to_string(),
                payee_name: snapshot.payee_name(payee).to_string(),
                category_id: category.to_string(),
                count,
            })
        })
        .collect()
}

/// The payee a rule is keyed on, when it has a `payee is <id>` condition.
fn payee_of_rule(rule: &Rule) -> Option<&str> {
    rule.conditions
        .iter()
        .find(|clause| clause.field == "payee" && clause.op == "is")
        .and_then(|clause| clause.value.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actual::{Payee, Transaction};

    fn tx(id: &str, payee: &str, category: Option<&str>, notes: Option<&str>) -> Transaction {
        Transaction {
            id: id.into(),
            account: "a1".into(),
            date: "2026-10-01".into(),
            amount: -100,
            payee: Some(payee.into()),
            imported_payee: None,
            imported_id: None,
            category: category.map(str::to_string),
            notes: notes.map(str::to_string),
            transfer_id: None,
            is_parent: false,
            is_child: false,
            starting_balance_flag: false,
        }
    }

    fn snapshot_of(transactions: Vec<Transaction>) -> Snapshot {
        Snapshot::new(
            vec![],
            vec![],
            vec![
                Payee {
                    id: "p1".into(),
                    name: "ICA".into(),
                },
                Payee {
                    id: "p2".into(),
                    name: "Coop".into(),
                },
            ],
            transactions,
        )
    }

    fn rule_for(payee: &str) -> Rule {
        Candidate {
            payee_id: payee.into(),
            payee_name: String::new(),
            category_id: "c9".into(),
            count: 0,
        }
        .rule()
    }

    #[test]
    fn earlier_tags_and_this_run_add_up() {
        let snapshot = snapshot_of(vec![tx("t1", "p1", Some("c1"), Some("#jev"))]);
        let now = [Applied {
            payee_id: "p1".into(),
            category_id: "c1".into(),
        }];
        let found = candidates(&snapshot, &now, &[], "#jev", 2);
        assert_eq!(
            found,
            vec![Candidate {
                payee_id: "p1".into(),
                payee_name: "ICA".into(),
                category_id: "c1".into(),
                count: 2,
            }]
        );
        assert!(candidates(&snapshot, &now, &[], "#jev", 3).is_empty());
    }

    #[test]
    fn disagreement_blocks_a_rule() {
        // Jev placed it in two categories over time.
        let snapshot = snapshot_of(vec![
            tx("t1", "p1", Some("c1"), Some("#jev")),
            tx("t2", "p1", Some("c2"), Some("#jev")),
        ]);
        assert!(candidates(&snapshot, &[], &[], "#jev", 1).is_empty());

        // A person put it somewhere else.
        let snapshot = snapshot_of(vec![
            tx("t1", "p1", Some("c1"), Some("#jev")),
            tx("t2", "p1", Some("c1"), Some("#jev")),
            tx("t3", "p1", Some("c2"), None),
        ]);
        assert!(candidates(&snapshot, &[], &[], "#jev", 1).is_empty());

        // A person agreeing is fine.
        let snapshot = snapshot_of(vec![
            tx("t1", "p1", Some("c1"), Some("#jev")),
            tx("t3", "p1", Some("c1"), None),
        ]);
        assert_eq!(candidates(&snapshot, &[], &[], "#jev", 1).len(), 1);
    }

    #[test]
    fn existing_rule_and_untagged_rows_are_skipped() {
        let snapshot = snapshot_of(vec![
            tx("t1", "p1", Some("c1"), Some("#jev")),
            tx("t2", "p2", Some("c1"), None),
        ]);
        assert!(candidates(&snapshot, &[], &[rule_for("p1")], "#jev", 1).is_empty());
        let found = candidates(&snapshot, &[], &[rule_for("p2")], "#jev", 1);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].payee_id, "p1");
    }

    #[test]
    fn rule_has_the_shape_actual_expects() {
        let json = serde_json::to_value(rule_for("p1")).unwrap();
        assert_eq!(json["stage"], serde_json::Value::Null);
        assert_eq!(json["conditionsOp"], "and");
        assert_eq!(json["conditions"][0]["field"], "payee");
        assert_eq!(json["conditions"][0]["op"], "is");
        assert_eq!(json["conditions"][0]["value"], "p1");
        assert_eq!(json["conditions"][0]["type"], "id");
        assert_eq!(json["actions"][0]["field"], "category");
        assert_eq!(json["actions"][0]["op"], "set");
        assert_eq!(json["actions"][0]["value"], "c9");
        assert!(json.get("id").is_none());
    }
}
