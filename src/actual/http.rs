//! `ActualStore` backed by actual-http-api, the Node sidecar that wraps `@actual-app/api`.

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue};
use serde::de::DeserializeOwned;
use serde::Deserialize;

use super::{
    Account, ActualStore, CategoryGroup, ImportOptions, ImportResult, NewTransaction, Payee,
    Transaction, TransactionPatch,
};

pub struct HttpStore {
    client: reqwest::Client,
    base_url: String,
    budget_sync_id: String,
}

#[derive(Deserialize)]
struct Envelope<T> {
    data: T,
}

impl HttpStore {
    pub fn new(
        base_url: &str,
        budget_sync_id: &str,
        api_key: &str,
        encryption_password: Option<&str>,
    ) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", HeaderValue::from_str(api_key)?);
        if let Some(password) = encryption_password {
            headers.insert(
                "budget-encryption-password",
                HeaderValue::from_str(password)?,
            );
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .context("building HTTP client for actual-http-api")?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            budget_sync_id: budget_sync_id.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!(
            "{}/v1/budgets/{}/{}",
            self.base_url, self.budget_sync_id, path
        )
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let url = self.url(path);
        let response = self
            .client
            .get(&url)
            .query(query)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("reading {url}"))?;
        anyhow::ensure!(status.is_success(), "GET {url} returned {status}: {body}");
        let envelope: Envelope<T> =
            serde_json::from_str(&body).with_context(|| format!("decoding {url}: {body}"))?;
        Ok(envelope.data)
    }
}

#[async_trait]
impl ActualStore for HttpStore {
    async fn accounts(&self) -> Result<Vec<Account>> {
        self.get("accounts", &[]).await
    }

    async fn category_groups(&self) -> Result<Vec<CategoryGroup>> {
        self.get("categorygroups", &[]).await
    }

    async fn payees(&self) -> Result<Vec<Payee>> {
        self.get("payees", &[]).await
    }

    async fn transactions(&self, account_id: &str, since: &str) -> Result<Vec<Transaction>> {
        self.get(
            &format!("accounts/{account_id}/transactions"),
            &[("since_date", since)],
        )
        .await
    }

    async fn update_transaction(&self, id: &str, patch: &TransactionPatch) -> Result<()> {
        let url = self.url(&format!("transactions/{id}"));
        let response = self
            .client
            .patch(&url)
            .json(&serde_json::json!({ "transaction": patch }))
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("PATCH {url} returned {status}: {body}");
        }
        Ok(())
    }

    async fn import_transactions(
        &self,
        account_id: &str,
        transactions: &[NewTransaction],
        options: &ImportOptions,
    ) -> Result<ImportResult> {
        let url = self.url(&format!("accounts/{account_id}/transactions/import"));
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "transactions": transactions,
                "dryRun": options.dry_run,
                "payeeNameNormalization": options.payee_normalization,
            }))
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .with_context(|| format!("reading {url}"))?;
        anyhow::ensure!(status.is_success(), "POST {url} returned {status}: {body}");
        let envelope: Envelope<ImportResult> =
            serde_json::from_str(&body).with_context(|| format!("decoding {url}: {body}"))?;
        Ok(envelope.data)
    }
}
