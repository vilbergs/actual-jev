//! `Classifier` backed by TypeSafe's System One API: one POST, one Choice question.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{Classifier, Decision, Subject, Taxonomy};

pub struct JevClient {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    instructions: String,
    max_retries: u32,
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    state: &'a Subject,
    questions: Questions<'a>,
}

#[derive(Serialize)]
struct Questions<'a> {
    category: ChoiceQuestion<'a>,
}

#[derive(Serialize)]
struct ChoiceQuestion<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    instructions: &'a str,
    criteria: BTreeMap<String, Option<String>>,
}

#[derive(Deserialize)]
struct Response {
    answers: Answers,
    #[serde(default)]
    usage: Usage,
}

#[derive(Deserialize)]
struct Answers {
    category: ChoiceAnswer,
}

#[derive(Deserialize)]
struct ChoiceAnswer {
    choice: String,
    confidence: f64,
    #[serde(default)]
    probabilities: BTreeMap<String, f64>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: u64,
}

impl JevClient {
    pub fn new(
        base_url: &str,
        api_key: &str,
        model: &str,
        instructions: &str,
        timeout: Duration,
        max_retries: u32,
    ) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .context("building HTTP client for Jev")?;
        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            instructions: instructions.to_string(),
            max_retries,
        })
    }

    async fn send(&self, request: &Request<'_>) -> Result<Response> {
        let url = format!("{}/v1/systemone", self.base_url);
        let mut backoff = Duration::from_millis(500);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(request)
                .send()
                .await;

            let retry_after = match outcome {
                Ok(response) if response.status().is_success() => {
                    let body = response.text().await.context("reading Jev response")?;
                    return serde_json::from_str(&body)
                        .with_context(|| format!("decoding Jev response: {body}"));
                }
                Ok(response) if is_retryable(response.status()) => {
                    let wait = retry_after_header(&response);
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    if attempt > self.max_retries {
                        anyhow::bail!("Jev returned {status} after {attempt} attempts: {body}");
                    }
                    warn!(%status, attempt, "Jev request failed, retrying");
                    wait
                }
                Ok(response) => {
                    let status = response.status();
                    let body = response.text().await.unwrap_or_default();
                    anyhow::bail!("Jev returned {status}: {body}");
                }
                Err(error) => {
                    if attempt > self.max_retries {
                        return Err(error).context("sending request to Jev");
                    }
                    warn!(%error, attempt, "Jev request failed, retrying");
                    None
                }
            };

            let wait = retry_after.unwrap_or(backoff).min(Duration::from_secs(60));
            tokio::time::sleep(wait).await;
            backoff = (backoff * 2).min(Duration::from_secs(5));
        }
    }
}

fn is_retryable(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retry_after_header(response: &reqwest::Response) -> Option<Duration> {
    let headers = response.headers();
    if let Some(ms) = headers.get("retry-after-ms") {
        return ms.to_str().ok()?.parse().ok().map(Duration::from_millis);
    }
    let seconds: u64 = headers.get("retry-after")?.to_str().ok()?.parse().ok()?;
    Some(Duration::from_secs(seconds))
}

#[async_trait]
impl Classifier for JevClient {
    async fn classify(&self, subject: &Subject, taxonomy: &Taxonomy) -> Result<Decision> {
        let request = Request {
            model: &self.model,
            state: subject,
            questions: Questions {
                category: ChoiceQuestion {
                    kind: "choice",
                    instructions: &self.instructions,
                    criteria: taxonomy.criteria(),
                },
            },
        };
        let response = self.send(&request).await?;
        let answer = response.answers.category;
        Ok(Decision {
            label: answer.choice,
            confidence: answer.confidence,
            probabilities: answer.probabilities,
            input_tokens: response.usage.input_tokens,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_matches_the_system_one_shape() {
        let subject = Subject {
            payee: "ICA".into(),
            imported_payee: "ICA KVANTUM".into(),
            amount: -847.5,
            direction: "outflow",
            date: "2026-10-08".into(),
            account: "Checking".into(),
            notes: String::new(),
        };
        let taxonomy = Taxonomy::new(vec![
            super::super::TaxonomyOption {
                label: "Groceries".into(),
                category_id: Some("c1".into()),
                description: Some("Supermarkets".into()),
            },
            super::super::TaxonomyOption {
                label: "Unsure".into(),
                category_id: None,
                description: None,
            },
        ]);
        let request = Request {
            model: "jev-latest",
            state: &subject,
            questions: Questions {
                category: ChoiceQuestion {
                    kind: "choice",
                    instructions: "Which category?",
                    criteria: taxonomy.criteria(),
                },
            },
        };
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["model"], "jev-latest");
        assert_eq!(json["state"]["payee"], "ICA");
        assert_eq!(json["state"]["amount"], -847.5);
        assert_eq!(json["questions"]["category"]["type"], "choice");
        assert_eq!(
            json["questions"]["category"]["criteria"]["Groceries"],
            "Supermarkets"
        );
        assert!(json["questions"]["category"]["criteria"]["Unsure"].is_null());
    }

    #[test]
    fn response_decodes_choice_answer() {
        let body = r#"{"model":"jev-1.13.0","answers":{"category":{"type":"choice",
            "choice":"Groceries","confidence":0.91,"probabilities":{"Groceries":0.91,"Unsure":0.09}}},
            "usage":{"input_tokens":412,"output_tokens":0}}"#;
        let response: Response = serde_json::from_str(body).unwrap();
        assert_eq!(response.answers.category.choice, "Groceries");
        assert_eq!(response.answers.category.confidence, 0.91);
        assert_eq!(response.usage.input_tokens, 412);
    }
}
