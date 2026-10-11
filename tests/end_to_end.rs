//! Drives the real binary against mock actual-http-api and Jev servers.

use std::collections::HashMap;
use std::process::Command;

use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const SYNC: &str = "b1";

fn jev_response(choice: &str, confidence: f64) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "model": "jev-1.13.0",
        "answers": { "category": {
            "type": "choice", "choice": choice, "confidence": confidence,
            "probabilities": { choice: confidence, "Unsure": 1.0 - confidence }
        }},
        "usage": { "input_tokens": 300, "output_tokens": 0 }
    }))
}

async fn mock_budget(actual: &MockServer, transactions: Value) {
    let base = format!("/v1/budgets/{SYNC}");
    Mock::given(method("GET"))
        .and(path(format!("{base}/accounts")))
        .and(header("x-api-key", "k"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [
            { "id": "a1", "name": "Checking", "offbudget": false, "closed": false },
            { "id": "a2", "name": "Pension", "offbudget": true, "closed": false }
        ]})))
        .mount(actual)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/categorygroups")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [
            { "id": "g1", "name": "Everyday", "is_income": false, "hidden": false, "categories": [
                { "id": "c1", "name": "Groceries", "group_id": "g1", "is_income": false, "hidden": false },
                { "id": "c2", "name": "Restaurants", "group_id": "g1", "is_income": false, "hidden": false }
            ]}
        ]})))
        .mount(actual)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/payees")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [
            { "id": "p1", "name": "ICA" }, { "id": "p2", "name": "Espresso House" }
        ]})))
        .mount(actual)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/accounts/a1/transactions")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": transactions })))
        .mount(actual)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/rules")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .with_priority(10)
        .mount(actual)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("{base}/accounts/a2/transactions")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .mount(actual)
        .await;
}

fn write_config(
    dir: &std::path::Path,
    actual: &MockServer,
    jev: &MockServer,
) -> std::path::PathBuf {
    let path = dir.join("jevcat.toml");
    std::fs::write(
        &path,
        format!(
            r#"
[actual]
base_url = "{}"
budget_sync_id = "{SYNC}"
since_days = 30

[jev]
base_url = "{}"
concurrency = 2
max_retries = 1

[jev.descriptions]
Groceries = "Supermarkets"

[policy]
apply_threshold = 0.85
learn_after = 1

[import]
account = "Checking"
delimiter = ";"
date_column = "Datum"
amount_column = "Belopp"
payee_column = "Text"
"#,
            actual.uri(),
            jev.uri()
        ),
    )
    .unwrap();
    path
}

fn jevcat(config: &std::path::Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_jevcat"))
        .arg("--config")
        .arg(config)
        .args(args)
        .env("ACTUAL_HTTP_API_KEY", "k")
        .env("TYPESAFE_API_KEY", "t")
        .env("RUST_LOG", "warn")
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

fn transactions() -> Value {
    json!([
        { "id": "t1", "account": "a1", "date": "2026-10-08", "amount": -84750, "payee": "p1",
          "imported_payee": "ICA KVANTUM", "category": null, "notes": null },
        { "id": "t2", "account": "a1", "date": "2026-10-07", "amount": -4500, "payee": "p2",
          "category": null, "notes": "fika" },
        { "id": "t3", "account": "a1", "date": "2026-10-06", "amount": -100, "payee": "p1",
          "category": "c1", "notes": null },
        { "id": "t4", "account": "a1", "date": "2026-10-05", "amount": -100, "payee": "p1",
          "category": null, "transfer_id": "x", "notes": null }
    ])
}

#[tokio::test]
async fn run_applies_flags_and_misses_by_confidence() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    mock_budget(&actual, transactions()).await;

    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(header("authorization", "Bearer t"))
        .and(body_partial_json(json!({ "state": { "payee": "ICA" } })))
        .respond_with(jev_response("Groceries", 0.93))
        .mount(&jev)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(body_partial_json(
            json!({ "state": { "payee": "Espresso House" } }),
        ))
        .respond_with(jev_response("Restaurants", 0.62))
        .mount(&jev)
        .await;

    let patches = Mock::given(method("PATCH"))
        .and(path(format!("/v1/budgets/{SYNC}/transactions/t1")))
        .and(body_partial_json(
            json!({ "transaction": { "category": "c1", "notes": "#jev" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "message": "ok" })))
        .expect(1)
        .named("apply t1");
    patches.mount(&actual).await;
    Mock::given(method("PATCH"))
        .and(path(format!("/v1/budgets/{SYNC}/transactions/t2")))
        .and(body_partial_json(
            json!({ "transaction": { "notes": "fika #jev-flag [jev: Restaurants 0.62]" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "message": "ok" })))
        .expect(1)
        .named("flag t2")
        .mount(&actual)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/v1/budgets/{SYNC}/rules")))
        .and(body_partial_json(json!({ "rule": {
            "stage": null,
            "conditionsOp": "and",
            "conditions": [{ "field": "payee", "op": "is", "value": "p1", "type": "id" }],
            "actions": [{ "field": "category", "op": "set", "value": "c1", "type": "id" }]
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "id": "r1" } })))
        .expect(1)
        .named("rule for ICA")
        .mount(&actual)
        .await;

    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);

    let (ok, out) = jevcat(&config, &["run"]);
    assert!(ok, "{out}");
    assert!(out.contains("Dry run"), "{out}");
    assert!(out.contains("APPLY"), "{out}");
    assert!(out.contains("FLAG"), "{out}");
    assert!(out.contains("RULE   ICA"), "{out}");
    assert!(
        out.contains("would write: 1 applied, 1 flagged, 0 missed, 0 failed, 1 rules"),
        "{out}"
    );
    assert_eq!(
        actual
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == "PATCH")
            .count(),
        0,
        "a dry run must not write"
    );
    assert_eq!(
        actual
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == "POST")
            .count(),
        0,
        "a dry run must not create rules"
    );

    let (ok, out) = jevcat(&config, &["run", "--apply"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("written: 1 applied, 1 flagged, 0 missed, 0 failed, 1 rules"),
        "{out}"
    );
    assert!(out.contains("skipped     1  transfer"), "{out}");
    assert!(out.contains("skipped     1  already categorized"), "{out}");
    actual.verify().await;

    let criteria: Vec<Request> = jev.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&criteria[0].body).unwrap();
    let options = body["questions"]["category"]["criteria"]
        .as_object()
        .unwrap();
    assert_eq!(options.len(), 3);
    assert_eq!(options["Groceries"], "Supermarkets");
    assert_eq!(options["Restaurants"], "Category group: Everyday");
    assert!(options.contains_key("Unsure"));
}

#[tokio::test]
async fn jev_retries_on_429_then_succeeds() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    mock_budget(
        &actual,
        json!([{ "id": "t1", "account": "a1", "date": "2026-10-08", "amount": -100,
                 "payee": "p1", "category": null }]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(ResponseTemplate::new(429).insert_header("retry-after-ms", "10"))
        .up_to_n_times(1)
        .mount(&jev)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(jev_response("Groceries", 0.9))
        .mount(&jev)
        .await;

    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);
    let (ok, out) = jevcat(&config, &["run"]);
    assert!(ok, "{out}");
    assert!(out.contains("would write: 1 applied"), "{out}");
    assert_eq!(jev.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn import_sends_rows_then_categorizes_only_the_added_ones() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    mock_budget(&actual, json!([])).await;

    // After the import, Actual holds the new row plus an old one that was matched.
    let after_import = json!([
        { "id": "new1", "account": "a1", "date": "2026-10-09", "amount": -12900, "payee": "p1",
          "imported_payee": "ICA NÄRA", "category": null, "notes": null },
        { "id": "old1", "account": "a1", "date": "2026-10-01", "amount": -500, "payee": "p2",
          "category": null, "notes": null }
    ]);
    Mock::given(method("POST"))
        .and(path(format!("/v1/budgets/{SYNC}/accounts/a1/transactions/import")))
        .and(body_partial_json(json!({
            "dryRun": false,
            "payeeNameNormalization": "title-case",
            "transactions": [
                { "date": "2026-10-09", "amount": -12900, "payee_name": "ICA NÄRA", "imported_payee": "ICA NÄRA" },
                { "date": "2026-10-01", "amount": -500, "payee_name": "ESPRESSO HOUSE" }
            ]
        })))
        .respond_with(move |_: &Request| {
            ResponseTemplate::new(201).set_body_json(json!({ "data": { "added": ["new1"], "updated": ["old1"] } }))
        })
        .expect(1)
        .mount(&actual)
        .await;
    // Outranks the empty transaction list mounted by mock_budget, so the reload after
    // the import sees the new rows. The run is limited to the ids the import added,
    // so old1 must stay untouched even though it is uncategorized.
    Mock::given(method("GET"))
        .and(path(format!("/v1/budgets/{SYNC}/accounts/a1/transactions")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": after_import })))
        .with_priority(1)
        .mount(&actual)
        .await;

    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .respond_with(jev_response("Groceries", 0.97))
        .expect(1)
        .mount(&jev)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/budgets/{SYNC}/rules")))
        .and(body_partial_json(json!({ "rule": {
            "conditions": [{ "field": "payee", "op": "is", "value": "p1" }],
            "actions": [{ "field": "category", "op": "set", "value": "c1" }]
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "id": "r1" } })))
        .expect(1)
        .mount(&actual)
        .await;
    Mock::given(method("PATCH"))
        .and(path(format!("/v1/budgets/{SYNC}/transactions/new1")))
        .and(body_partial_json(
            json!({ "transaction": { "category": "c1", "notes": "#jev" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "message": "ok" })))
        .expect(1)
        .mount(&actual)
        .await;

    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);
    let csv = dir.join("statement.csv");
    std::fs::write(
        &csv,
        "Datum;Belopp;Text\n2026-10-09;-129,00;ICA NÄRA\n2026-10-01;-5,00;ESPRESSO HOUSE\n",
    )
    .unwrap();

    let (ok, out) = jevcat(&config, &["import", csv.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert!(out.contains("2 rows"), "{out}");
    assert!(out.contains("Import: 1 added, 1 matched existing"), "{out}");
    assert!(
        out.contains("written: 1 applied, 0 flagged, 0 missed, 0 failed, 1 rules"),
        "{out}"
    );
    actual.verify().await;
    jev.verify().await;
}

#[tokio::test]
async fn import_dry_run_writes_nothing() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    mock_budget(&actual, json!([])).await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1/budgets/{SYNC}/accounts/a1/transactions/import"
        )))
        .and(body_partial_json(json!({ "dryRun": true })))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({ "data": { "added": ["would-be"], "updated": [] } })),
        )
        .expect(1)
        .mount(&actual)
        .await;

    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);
    let csv = dir.join("statement.csv");
    std::fs::write(&csv, "Datum;Belopp;Text\n2026-10-09;-129,00;ICA\n").unwrap();

    let (ok, out) = jevcat(
        &config,
        &[
            "import",
            csv.to_str().unwrap(),
            "--dry-run",
            "--account",
            "checking",
        ],
    );
    assert!(ok, "{out}");
    assert!(out.contains("Import: 1 added"), "{out}");
    assert!(out.contains("skipped on a dry run"), "{out}");
    assert!(jev.received_requests().await.unwrap().is_empty());
    assert_eq!(
        actual
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method == "PATCH")
            .count(),
        0
    );
}

#[tokio::test]
async fn calibrate_reports_buckets_and_a_threshold() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    mock_budget(
        &actual,
        json!([
            { "id": "t1", "account": "a1", "date": "2026-10-08", "amount": -100, "payee": "p1", "category": "c1" },
            { "id": "t2", "account": "a1", "date": "2026-10-07", "amount": -100, "payee": "p2", "category": "c2" },
            { "id": "t3", "account": "a1", "date": "2026-10-06", "amount": -100, "payee": "p1", "category": null }
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(body_partial_json(json!({ "state": { "payee": "ICA" } })))
        .respond_with(jev_response("Groceries", 0.95))
        .mount(&jev)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/systemone"))
        .and(body_partial_json(
            json!({ "state": { "payee": "Espresso House" } }),
        ))
        .respond_with(jev_response("Groceries", 0.55))
        .mount(&jev)
        .await;

    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);
    let (ok, out) = jevcat(&config, &["calibrate", "--target", "0.9"]);
    assert!(ok, "{out}");
    assert!(out.contains("2 samples"), "{out}");
    assert!(
        out.contains("Suggested apply_threshold for 90% precision: 0.6"),
        "{out}"
    );
    assert!(out.contains("Restaurants -> Groceries"), "{out}");
    assert_eq!(
        jev.received_requests().await.unwrap().len(),
        2,
        "uncategorized rows are not replayed"
    );
}

#[tokio::test]
async fn list_needs_no_jev_and_secrets_are_required() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    mock_budget(&actual, transactions()).await;
    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);

    let (ok, out) = jevcat(&config, &["list"]);
    assert!(ok, "{out}");
    assert!(
        out.contains("2 eligible transactions, 2 categories"),
        "{out}"
    );
    assert!(jev.received_requests().await.unwrap().is_empty());

    let output = Command::new(env!("CARGO_BIN_EXE_jevcat"))
        .arg("--config")
        .arg(&config)
        .arg("list")
        .env_clear()
        .envs(HashMap::from([(
            "PATH",
            std::env::var("PATH").unwrap_or_default(),
        )]))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ACTUAL_HTTP_API_KEY"));
}

fn tempdir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "jevcat-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn learn_creates_rules_from_earlier_tags_and_skips_covered_payees() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    mock_budget(
        &actual,
        json!([
            // ICA: applied by Jev twice, agrees -> rule
            { "id": "t1", "account": "a1", "date": "2026-10-08", "amount": -100, "payee": "p1", "category": "c1", "notes": "#jev" },
            { "id": "t2", "account": "a1", "date": "2026-10-07", "amount": -100, "payee": "p1", "category": "c1", "notes": "weekly #jev" },
            // Espresso House: applied twice but a person moved one -> no rule
            { "id": "t3", "account": "a1", "date": "2026-10-06", "amount": -100, "payee": "p2", "category": "c2", "notes": "#jev" },
            { "id": "t4", "account": "a1", "date": "2026-10-05", "amount": -100, "payee": "p2", "category": "c2", "notes": "#jev" },
            { "id": "t5", "account": "a1", "date": "2026-10-04", "amount": -100, "payee": "p2", "category": "c1", "notes": null }
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/budgets/{SYNC}/rules")))
        .and(body_partial_json(json!({ "rule": {
            "conditions": [{ "field": "payee", "op": "is", "value": "p1" }],
            "actions": [{ "field": "category", "op": "set", "value": "c1" }]
        }})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "id": "r1" } })))
        .expect(1)
        .mount(&actual)
        .await;

    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);

    let (ok, out) = jevcat(&config, &["learn", "--after", "2"]);
    assert!(ok, "{out}");
    assert!(out.contains("RULE   ICA"), "{out}");
    assert!(!out.contains("Espresso"), "{out}");
    assert!(out.contains("would create 1 rules"), "{out}");

    let (ok, out) = jevcat(&config, &["learn", "--after", "2", "--apply"]);
    assert!(ok, "{out}");
    assert!(out.contains("created 1 rules"), "{out}");
    actual.verify().await;
    assert!(jev.received_requests().await.unwrap().is_empty());

    // Once a rule covers ICA, nothing is proposed.
    Mock::given(method("GET"))
        .and(path(format!("/v1/budgets/{SYNC}/rules")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [
            { "id": "r1", "stage": null, "conditionsOp": "and",
              "conditions": [{ "field": "payee", "op": "is", "value": "p1", "type": "id" }],
              "actions": [{ "field": "category", "op": "set", "value": "c1", "type": "id" }] }
        ]})))
        .with_priority(1)
        .mount(&actual)
        .await;
    let (ok, out) = jevcat(&config, &["learn", "--after", "2"]);
    assert!(ok, "{out}");
    assert!(out.contains("would create 0 rules"), "{out}");
}

#[tokio::test]
async fn import_drops_ids_where_rows_overlap_synced_history_and_honours_bounds() {
    let actual = MockServer::start().await;
    let jev = MockServer::start().await;
    // The account holds synced rows (they carry the bank's imported_id) for October.
    mock_budget(
        &actual,
        json!([
            { "id": "s1", "account": "a1", "date": "2026-10-03", "amount": -500, "payee": "p2",
              "imported_id": "bank-1", "category": "c2" },
            { "id": "s2", "account": "a1", "date": "2026-10-09", "amount": -12900, "payee": "p1",
              "imported_id": "bank-2", "category": "c1" }
        ]),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!(
            "/v1/budgets/{SYNC}/accounts/a1/transactions/import"
        )))
        .respond_with(
            ResponseTemplate::new(201)
                .set_body_json(json!({ "data": { "added": [], "updated": ["s2"] } })),
        )
        .expect(1)
        .mount(&actual)
        .await;

    let dir = tempdir();
    let config = write_config(&dir, &actual, &jev);
    let csv = dir.join("statement.csv");
    std::fs::write(
        &csv,
        "Datum;Belopp;Text\n\
         2026-08-15;-10,00;OLD\n\
         2026-09-20;-20,00;GAP\n\
         2026-10-09;-129,00;ICA NÄRA\n\
         2026-10-20;-30,00;FUTURE\n",
    )
    .unwrap();

    let (ok, out) = jevcat(
        &config,
        &[
            "import",
            csv.to_str().unwrap(),
            "--from",
            "2026-09-01",
            "--until",
            "2026-10-15",
            "--no-categorize",
        ],
    );
    assert!(ok, "{out}");
    assert!(out.contains("from 2026-10-03 to 2026-10-09"), "{out}");
    assert!(
        out.contains("Skipping 1 rows before --from and 1 rows after --until"),
        "{out}"
    );
    assert!(out.contains("1 rows overlap that history"), "{out}");

    let import = actual
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .find(|r| r.method == "POST" && r.url.path().ends_with("/import"))
        .unwrap();
    let body: Value = serde_json::from_slice(&import.body).unwrap();
    let rows = body["transactions"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "{body}");
    assert_eq!(rows[0]["date"], "2026-09-20");
    assert!(
        rows[0]["imported_id"]
            .as_str()
            .unwrap()
            .starts_with("jevcat-"),
        "gap row keeps its id"
    );
    assert_eq!(rows[1]["date"], "2026-10-09");
    assert!(
        rows[1].get("imported_id").is_none(),
        "overlapping row is matched by amount and date: {body}"
    );
}
