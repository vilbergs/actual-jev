//! The `ActualStore` trait and the budget types the rest of the tool works with.
//!
//! Everything above this module talks to the trait only, so the backend can change
//! from the HTTP sidecar to a native sync client without touching the run loop.

pub mod http;

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize};

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "flag")]
    pub offbudget: bool,
    #[serde(default, deserialize_with = "flag")]
    pub closed: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct CategoryGroup {
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "flag")]
    pub is_income: bool,
    #[serde(default, deserialize_with = "flag")]
    pub hidden: bool,
    #[serde(default)]
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Category {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    pub is_income: bool,
    #[serde(default, deserialize_with = "flag")]
    pub hidden: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Payee {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// A transaction as Actual's API returns it. `amount` is in minor units (öre, cents).
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Transaction {
    pub id: String,
    pub account: String,
    pub date: String,
    #[serde(default)]
    pub amount: i64,
    #[serde(default)]
    pub payee: Option<String>,
    #[serde(default)]
    pub imported_payee: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub transfer_id: Option<String>,
    #[serde(default, deserialize_with = "flag")]
    pub is_parent: bool,
    #[serde(default, deserialize_with = "flag")]
    pub is_child: bool,
    #[serde(default, deserialize_with = "flag")]
    pub starting_balance_flag: bool,
}

/// The fields a run is allowed to change on a transaction.
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
pub struct TransactionPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// A transaction to import, in the shape Actual's `importTransactions` expects.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct NewTransaction {
    pub date: String,
    /// Minor units, negative for outflows.
    pub amount: i64,
    pub payee_name: String,
    pub imported_payee: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Stable per bank row, so a re-import of the same file adds nothing twice.
    pub imported_id: String,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ImportResult {
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub updated: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportOptions {
    pub dry_run: bool,
    /// `title-case` or `original`.
    pub payee_normalization: String,
}

#[async_trait]
pub trait ActualStore: Send + Sync {
    async fn accounts(&self) -> Result<Vec<Account>>;
    async fn category_groups(&self) -> Result<Vec<CategoryGroup>>;
    async fn payees(&self) -> Result<Vec<Payee>>;
    /// Transactions of one account from `since` (YYYY-MM-DD) until today.
    async fn transactions(&self, account_id: &str, since: &str) -> Result<Vec<Transaction>>;
    async fn update_transaction(&self, id: &str, patch: &TransactionPatch) -> Result<()>;
    /// Runs Actual's own import: rules apply, and rows already present are matched
    /// instead of duplicated.
    async fn import_transactions(
        &self,
        account_id: &str,
        transactions: &[NewTransaction],
        options: &ImportOptions,
    ) -> Result<ImportResult>;
}

/// One read of everything a run needs, so lookups during the run are in memory.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub accounts: Vec<Account>,
    pub groups: Vec<CategoryGroup>,
    pub payees: Vec<Payee>,
    pub transactions: Vec<Transaction>,
    account_names: HashMap<String, String>,
    payee_names: HashMap<String, String>,
}

impl Snapshot {
    pub async fn load(store: &dyn ActualStore, since: &str) -> Result<Self> {
        let accounts = store.accounts().await?;
        let groups = store.category_groups().await?;
        let payees = store.payees().await?;
        let mut transactions = Vec::new();
        for account in accounts.iter().filter(|account| !account.closed) {
            transactions.extend(store.transactions(&account.id, since).await?);
        }
        Ok(Self::new(accounts, groups, payees, transactions))
    }

    pub fn new(
        accounts: Vec<Account>,
        groups: Vec<CategoryGroup>,
        payees: Vec<Payee>,
        transactions: Vec<Transaction>,
    ) -> Self {
        let account_names = accounts
            .iter()
            .map(|account| (account.id.clone(), account.name.clone()))
            .collect();
        let payee_names = payees
            .iter()
            .map(|payee| (payee.id.clone(), payee.name.clone()))
            .collect();
        Self {
            accounts,
            groups,
            payees,
            transactions,
            account_names,
            payee_names,
        }
    }

    pub fn account_name(&self, id: &str) -> &str {
        self.account_names.get(id).map(String::as_str).unwrap_or("")
    }

    pub fn payee_name(&self, id: &str) -> &str {
        self.payee_names.get(id).map(String::as_str).unwrap_or("")
    }

    /// Finds an account by id or by case-insensitive name.
    pub fn account(&self, name_or_id: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| {
            account.id == name_or_id || account.name.eq_ignore_ascii_case(name_or_id)
        })
    }
}

/// Actual's API has returned booleans as `true`, `1` and `null` depending on the path
/// a row took, so accept all three.
fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Flag {
        Bool(bool),
        Int(i64),
        Null(()),
    }
    Ok(match Flag::deserialize(deserializer)? {
        Flag::Bool(value) => value,
        Flag::Int(value) => value != 0,
        Flag::Null(_) => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_accept_bool_int_and_null() {
        let tx: Transaction = serde_json::from_str(
            r#"{"id":"t","account":"a","date":"2026-10-01","amount":-100,
                "is_parent":1,"is_child":false,"starting_balance_flag":null}"#,
        )
        .unwrap();
        assert!(tx.is_parent);
        assert!(!tx.is_child);
        assert!(!tx.starting_balance_flag);
    }

    #[test]
    fn patch_serializes_only_set_fields() {
        let patch = TransactionPatch {
            category: Some("c1".into()),
            notes: None,
        };
        assert_eq!(
            serde_json::to_string(&patch).unwrap(),
            r#"{"category":"c1"}"#
        );
    }
}
