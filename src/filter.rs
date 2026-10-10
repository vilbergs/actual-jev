//! Which transactions a run may touch. Everything here is a reason to leave one alone.

use std::collections::HashSet;
use std::fmt;

use crate::actual::{Snapshot, Transaction};
use crate::tags;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Skip {
    HasCategory,
    Transfer,
    StartingBalance,
    SplitParent,
    NoPayee,
    OffBudgetAccount,
    MissedBefore,
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Skip::HasCategory => "already categorized",
            Skip::Transfer => "transfer",
            Skip::StartingBalance => "starting balance",
            Skip::SplitParent => "split parent",
            Skip::NoPayee => "no payee",
            Skip::OffBudgetAccount => "off-budget account",
            Skip::MissedBefore => "marked as missed",
        })
    }
}

pub struct Filter<'a> {
    off_budget: HashSet<&'a str>,
    missed_tag: &'a str,
    rerun_missed: bool,
}

impl<'a> Filter<'a> {
    pub fn new(snapshot: &'a Snapshot, missed_tag: &'a str, rerun_missed: bool) -> Self {
        Self {
            off_budget: snapshot
                .accounts
                .iter()
                .filter(|account| account.offbudget)
                .map(|account| account.id.as_str())
                .collect(),
            missed_tag,
            rerun_missed,
        }
    }

    /// `Ok` when the transaction should be classified; `Err` says why not.
    pub fn check(&self, tx: &Transaction) -> Result<(), Skip> {
        self.check_shape(tx)?;
        if tx.category.is_some() {
            return Err(Skip::HasCategory);
        }
        let notes = tx.notes.as_deref().unwrap_or("");
        if !self.rerun_missed && tags::has_tag(notes, self.missed_tag) {
            return Err(Skip::MissedBefore);
        }
        Ok(())
    }

    /// The shape checks alone, for calibration over transactions that have a category.
    pub fn check_shape(&self, tx: &Transaction) -> Result<(), Skip> {
        if tx.transfer_id.as_deref().is_some_and(|id| !id.is_empty()) {
            return Err(Skip::Transfer);
        }
        if tx.starting_balance_flag {
            return Err(Skip::StartingBalance);
        }
        if tx.is_parent {
            return Err(Skip::SplitParent);
        }
        if self.off_budget.contains(tx.account.as_str()) {
            return Err(Skip::OffBudgetAccount);
        }
        let has_payee = tx.payee.as_deref().is_some_and(|p| !p.is_empty())
            || tx.imported_payee.as_deref().is_some_and(|p| !p.is_empty());
        if !has_payee {
            return Err(Skip::NoPayee);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actual::Account;

    fn tx() -> Transaction {
        Transaction {
            id: "t1".into(),
            account: "a1".into(),
            date: "2026-10-01".into(),
            amount: -1000,
            payee: Some("p1".into()),
            imported_payee: None,
            category: None,
            notes: None,
            transfer_id: None,
            is_parent: false,
            is_child: false,
            starting_balance_flag: false,
        }
    }

    fn snapshot() -> Snapshot {
        Snapshot::new(
            vec![
                Account {
                    id: "a1".into(),
                    name: "Checking".into(),
                    offbudget: false,
                    closed: false,
                },
                Account {
                    id: "a2".into(),
                    name: "Pension".into(),
                    offbudget: true,
                    closed: false,
                },
            ],
            vec![],
            vec![],
            vec![],
        )
    }

    #[test]
    fn eligible_transaction_passes() {
        let snapshot = snapshot();
        let filter = Filter::new(&snapshot, "#jev-miss", false);
        assert_eq!(filter.check(&tx()), Ok(()));
    }

    #[test]
    fn each_reason_is_reported() {
        let snapshot = snapshot();
        let filter = Filter::new(&snapshot, "#jev-miss", false);

        let mut t = tx();
        t.category = Some("c".into());
        assert_eq!(filter.check(&t), Err(Skip::HasCategory));

        let mut t = tx();
        t.transfer_id = Some("x".into());
        assert_eq!(filter.check(&t), Err(Skip::Transfer));

        let mut t = tx();
        t.is_parent = true;
        assert_eq!(filter.check(&t), Err(Skip::SplitParent));

        let mut t = tx();
        t.account = "a2".into();
        assert_eq!(filter.check(&t), Err(Skip::OffBudgetAccount));

        let mut t = tx();
        t.payee = None;
        assert_eq!(filter.check(&t), Err(Skip::NoPayee));
        t.imported_payee = Some("RAW PAYEE".into());
        assert_eq!(filter.check(&t), Ok(()));

        let mut t = tx();
        t.notes = Some("lunch #jev-miss".into());
        assert_eq!(filter.check(&t), Err(Skip::MissedBefore));
        let rerun = Filter::new(&snapshot, "#jev-miss", true);
        assert_eq!(rerun.check(&t), Ok(()));
    }
}
