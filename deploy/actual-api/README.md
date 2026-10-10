# actual-http-api on Fly.io

Runs [actual-http-api](https://github.com/jhonderson/actual-http-api) as a second Fly app
next to your Actual server, so jevcat can reach the budget over HTTP. Traffic between the
two apps stays on Fly's private network.

## Deploy

Edit the placeholders at the top of `fly.toml`: the app name, the region (use the same one
as your Actual app) and the Fly app name of your Actual instance in `ACTUAL_SERVER_URL`.
Pin the image tag to the version your Actual server runs; the API client inside must match it.

```sh
cd deploy/actual-api
fly launch --copy-config --no-deploy          # registers the app from fly.toml
fly volumes create actual_api_data --size 1   # cache for the downloaded budget
fly secrets set \
  API_KEY="$(openssl rand -hex 32)" \
  ACTUAL_SERVER_PASSWORD="<your Actual server password>"
fly deploy
```

Keep the generated `API_KEY`. It only authenticates callers of this sidecar and is what
jevcat sends in the `x-api-key` header.

## Check it

```sh
curl -s https://actual-api.fly.dev/v1/budgets/<sync id>/accounts -H "x-api-key: <API_KEY>"
```

Your accounts should come back as JSON. The Swagger UI is at `/api-docs/`. The first
request after the machine has slept takes a few seconds while it boots and pulls the budget.

## Point jevcat at it

```toml
[actual]
base_url = "https://actual-api.fly.dev"
budget_sync_id = "<sync id>"
```

```sh
export ACTUAL_HTTP_API_KEY="<API_KEY>"
export ACTUAL_ENCRYPTION_PASSWORD="…"   # only for an end-to-end encrypted budget
jevcat list
```

## Keeping it off the public internet

Delete the `[http_service]` block, redeploy, and open a tunnel while you run jevcat:

```sh
fly proxy 5007 -a actual-api
```

with `base_url = "http://localhost:5007"` in `jevcat.toml`.

## If `.internal` does not resolve

Use your Actual instance's public URL instead: `ACTUAL_SERVER_URL = "https://your-actual-app.fly.dev/"`.
