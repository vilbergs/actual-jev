//! The `Classifier` trait, the taxonomy it chooses from, and the subject it judges.

pub mod jev;

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use async_trait::async_trait;
use serde::Serialize;

use crate::actual::{CategoryGroup, Snapshot, Transaction};

/// One option the classifier may pick. `category_id` is `None` for the escape hatch.
#[derive(Debug, Clone, PartialEq)]
pub struct TaxonomyOption {
    pub label: String,
    pub category_id: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Taxonomy {
    pub options: Vec<TaxonomyOption>,
    by_label: HashMap<String, usize>,
}

impl Taxonomy {
    /// Builds the option list from Actual's category groups. Hidden groups and hidden
    /// categories are left out. A category name that repeats across groups is
    /// disambiguated as `Name (Group)` so every label stays unique.
    pub fn from_groups(
        groups: &[CategoryGroup],
        descriptions: &HashMap<String, String>,
        unsure_label: &str,
    ) -> Self {
        let mut name_counts: HashMap<&str, usize> = HashMap::new();
        for group in groups.iter().filter(|group| !group.hidden) {
            for category in group.categories.iter().filter(|category| !category.hidden) {
                *name_counts.entry(category.name.as_str()).or_default() += 1;
            }
        }

        let mut options = Vec::new();
        for group in groups.iter().filter(|group| !group.hidden) {
            for category in group.categories.iter().filter(|category| !category.hidden) {
                let label = if name_counts[category.name.as_str()] > 1 {
                    format!("{} ({})", category.name, group.name)
                } else {
                    category.name.clone()
                };
                let description = descriptions
                    .get(&label)
                    .or_else(|| descriptions.get(&category.name))
                    .cloned()
                    .unwrap_or_else(|| format!("Category group: {}", group.name));
                options.push(TaxonomyOption {
                    label,
                    category_id: Some(category.id.clone()),
                    description: Some(description),
                });
            }
        }
        options.push(TaxonomyOption {
            label: unsure_label.to_string(),
            category_id: None,
            description: Some("Does not clearly fit any category above".to_string()),
        });
        Self::new(options)
    }

    pub fn new(options: Vec<TaxonomyOption>) -> Self {
        let by_label = options
            .iter()
            .enumerate()
            .map(|(index, option)| (option.label.clone(), index))
            .collect();
        Self { options, by_label }
    }

    pub fn option(&self, label: &str) -> Option<&TaxonomyOption> {
        self.by_label.get(label).map(|index| &self.options[*index])
    }

    pub fn label_for_category(&self, category_id: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|option| option.category_id.as_deref() == Some(category_id))
            .map(|option| option.label.as_str())
    }

    /// The question's criteria as Jev expects them: label to rubric.
    pub fn criteria(&self) -> BTreeMap<String, Option<String>> {
        self.options
            .iter()
            .map(|option| (option.label.clone(), option.description.clone()))
            .collect()
    }
}

/// What the classifier sees of a transaction. Sent as a JSON object, not a prompt.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Subject {
    pub payee: String,
    pub imported_payee: String,
    pub amount: f64,
    pub direction: &'static str,
    pub date: String,
    pub account: String,
    pub notes: String,
}

impl Subject {
    pub fn from_transaction(tx: &Transaction, snapshot: &Snapshot) -> Self {
        let payee = tx
            .payee
            .as_deref()
            .map(|id| snapshot.payee_name(id))
            .unwrap_or("")
            .to_string();
        Self {
            payee,
            imported_payee: tx.imported_payee.clone().unwrap_or_default(),
            amount: tx.amount as f64 / 100.0,
            direction: if tx.amount >= 0 { "inflow" } else { "outflow" },
            date: tx.date.clone(),
            account: snapshot.account_name(&tx.account).to_string(),
            notes: crate::tags::strip(tx.notes.as_deref().unwrap_or(""), "#jev"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub label: String,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
    pub input_tokens: u64,
}

#[async_trait]
pub trait Classifier: Send + Sync {
    async fn classify(&self, subject: &Subject, taxonomy: &Taxonomy) -> Result<Decision>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actual::Category;

    fn group(name: &str, categories: &[(&str, &str)]) -> CategoryGroup {
        CategoryGroup {
            id: format!("g-{name}"),
            name: name.to_string(),
            is_income: false,
            hidden: false,
            categories: categories
                .iter()
                .map(|(id, name)| Category {
                    id: id.to_string(),
                    name: name.to_string(),
                    group_id: Some(format!("g-{name}")),
                    is_income: false,
                    hidden: false,
                })
                .collect(),
        }
    }

    #[test]
    fn duplicate_names_get_group_suffix_and_unsure_is_last() {
        let groups = vec![
            group("Home", &[("c1", "Insurance"), ("c2", "Groceries")]),
            group("Car", &[("c3", "Insurance")]),
        ];
        let mut descriptions = HashMap::new();
        descriptions.insert("Groceries".to_string(), "Supermarkets".to_string());
        let taxonomy = Taxonomy::from_groups(&groups, &descriptions, "Unsure");

        let labels: Vec<&str> = taxonomy.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Insurance (Home)", "Groceries", "Insurance (Car)", "Unsure"]
        );
        assert_eq!(
            taxonomy.option("Groceries").unwrap().description.as_deref(),
            Some("Supermarkets")
        );
        assert_eq!(
            taxonomy
                .option("Insurance (Car)")
                .unwrap()
                .description
                .as_deref(),
            Some("Category group: Car")
        );
        assert_eq!(taxonomy.option("Unsure").unwrap().category_id, None);
        assert_eq!(taxonomy.label_for_category("c3"), Some("Insurance (Car)"));
    }

    #[test]
    fn hidden_categories_are_left_out() {
        let mut groups = vec![group("Home", &[("c1", "Old"), ("c2", "Rent")])];
        groups[0].categories[0].hidden = true;
        let taxonomy = Taxonomy::from_groups(&groups, &HashMap::new(), "Unsure");
        assert!(taxonomy.option("Old").is_none());
        assert!(taxonomy.option("Rent").is_some());
    }
}
