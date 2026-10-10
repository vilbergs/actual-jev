# jevcat

Categorizes [Actual Budget](https://actualbudget.org/) transactions with
[TypeSafe Jev](https://typesafe.ai), a decision model that answers a multiple-choice
question with a calibrated confidence instead of generating text. Every category is an
option, `Unsure` is always one of them, and the confidence decides whether a category is
written, flagged for review, or left alone.

Run it by hand. There is no daemon and no schedule.

```
jevcat import statement.csv      import a bank CSV into Actual, then categorize what it added
jevcat run                       categorize what is already uncategorized (dry run)
jevcat run --apply               ...and write it
jevcat calibrate                 measure accuracy per confidence on your categorized history
jevcat list                      show what a run would look at, without calling Jev
```

## How it reaches Actual

Actual's server is a sync log, not a REST API, so the binary talks to
[actual-http-api](https://github.com/jhonderson/actual-http-api), a small container that
wraps the official `@actual-app/api` and exposes it over HTTP. Run it next to your Actual
server:

```sh
docker run -d --name actual-http-api \
  -p 5007:5007 \
  -e ACTUAL_SERVER_URL="http://actual-server:5006/" \
  -e ACTUAL_SERVER_PASSWORD="…" \
  -e API_KEY="pick-a-long-random-string" \
  jhonderson/actual-http-api:26.10.0
```

Everything that touches Actual goes through the `ActualStore` trait in `src/actual`, so a
native Rust sync client can replace the sidecar later without changing anything else.

## Setup

```sh
cargo install --path .
cp jevcat.example.toml jevcat.toml      # fill in base_url, budget_sync_id, [import] columns
export ACTUAL_HTTP_API_KEY=…            # the API_KEY above
export TYPESAFE_API_KEY=…
export ACTUAL_ENCRYPTION_PASSWORD=…     # only for an end-to-end encrypted budget
jevcat list
```

Then calibrate before trusting it:

```sh
jevcat calibrate --limit 300
```

It replays your most recent categorized transactions through Jev and prints accuracy per
confidence bucket plus a suggested `apply_threshold` for 95% precision. Put that number in
the config. If no threshold reaches the target, improve `[jev.descriptions]` and rerun;
the rubric text is what Jev reads.

## Importing a CSV

```sh
jevcat import ~/Downloads/statement.csv --account Checking
```

1. The CSV is parsed with the column names, delimiter and date format from `[import]`.
   Amounts like `-1 234,56`, `-1.234,56` and `1,234.56` are all read correctly.
2. Rows go through Actual's own import, which runs your rules and de-duplicates on
   `imported_id` (a bank id column if you name one, otherwise a stable hash of the row).
   Re-importing the same file adds nothing.
3. Only the rows Actual reports as added are classified, and the result is written.

`--dry-run` parses and sends the import with Actual's dry-run flag, so you see what
would be added without changing the budget. `--no-categorize` imports only.

## What a run writes

| Jev says | Confidence | Action | Note tag |
|---|---|---|---|
| a category | at or above threshold | category set | `#jev` |
| a category | below threshold | left uncategorized | `#jev-flag [jev: Groceries 0.71]` |
| `Unsure` | any | left uncategorized, not retried | `#jev-miss` |

Search `#jev-flag` in Actual to review the guesses. Tags are the only state the tool
keeps; `--rerun-missed` retries the `#jev-miss` ones.

Skipped and never touched: transfers, split parents, starting balances, off-budget
accounts, rows with no payee, and anything that already has a category.

## Layout

| Module | Role |
|---|---|
| `actual` | `ActualStore` trait, budget types, `Snapshot` |
| `actual::http` | the actual-http-api backend |
| `classify` | `Classifier` trait, `Taxonomy` built from category groups, `Subject` sent to the model |
| `classify::jev` | one `POST /v1/systemone` with retries |
| `import` | CSV parsing to Actual's import shape |
| `filter` | eligibility rules |
| `policy` | decision to apply / flag / skip |
| `calibrate` | accuracy per confidence bucket, threshold suggestion |
| `run` | the pass over the budget |
| `tags` | note tag handling |

`cargo test` runs the unit tests; `tests/` holds end-to-end tests against mock HTTP servers.
