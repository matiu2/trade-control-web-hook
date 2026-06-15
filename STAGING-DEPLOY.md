# Staging deploy — step list

Run these from the repo root **on the `staging` branch**. Steps marked
**[you]** need your Cloudflare auth — run them yourself (e.g. type
`! wrangler …` in the Claude session so the output is captured, or in your
own terminal).

```sh
git checkout staging
```

## 1. Create the staging KV namespace  **[you]**

```sh
wrangler kv namespace create TRADE_CONTROL_KV_STAGING
```

Copy the printed `id` into `wrangler.toml` on the `staging` branch,
replacing `REPLACE_ME_STAGING_KV_ID`:

```toml
[[kv_namespaces]]
binding = "TRADE_CONTROL_KV"
id = "<paste the id here>"
```

## 2. Create the staging R2 bucket  **[you]**

```sh
wrangler r2 bucket create trade-control-recording-staging
```

### If this fails with a permissions / authorization error

The worker needs R2, and so does the **API token wrangler deploys with**.
R2 is *not* included in the default Workers token scope. Fix one of:

- **OAuth login (simplest):** `wrangler login` and ensure the browser
  consent includes R2. Re-running `wrangler login` re-prompts for scopes.
- **API token:** in the Cloudflare dashboard → My Profile → API Tokens,
  edit the token wrangler uses to add **`Workers R2 Storage: Edit`**
  (and keep `Workers Scripts: Edit`, `Workers KV Storage: Edit`).
- Confirm R2 is enabled on the account at all (dashboard → R2 → it may
  ask you to accept R2 terms once; free tier is fine).

`wrangler r2 bucket list` should then show the bucket. The bucket create
and the first `put` from the deployed worker both need this scope — if the
bucket creates but recording silently does nothing after deploy, check the
worker logs for `recording: R2 put failed:` (that's the token/scope, or a
binding typo).

## 3. Set the staging secrets  **[you]**

The staging worker is a *separate* worker, so it needs its own secrets.
At minimum the signing key and the TradeNation demo account:

```sh
wrangler secret put SIGNING_KEY < ~/.config/trade-control/key.hex
wrangler secret put TN_ACCOUNT_<DEMO_NAME>     # paste the credentials blob
# plus any of: MAX_RISK_PCT_PER_TRADE, MAX_OPEN_POSITIONS, PIP_SIZE_*
```

> `wrangler secret put` targets the worker named in `wrangler.toml` on the
> current branch — so being on `staging` (name
> `trade-control-web-hook-staging`) is what makes these staging secrets.

## 4. Deploy  **[you]**

```sh
./deploy.sh        # wrangler deploy + reinstall tv-arm/cli/tv-news
# or just: wrangler deploy
```

The deploy URL will be `trade-control-web-hook-staging.<subdomain>.workers.dev`.
Point your **staging** TradingView alerts / `tv-arm-staging` at that URL.

## 5. Verify recording works

After the first real (or test) intent fires:

```sh
wrangler tail                                   # watch live logs
wrangler r2 object list trade-control-recording-staging --prefix req/
```

You should see `req/<date>/<ts>-<request_id>.json` objects appearing. Pull
one to eyeball it:

```sh
wrangler r2 object get trade-control-recording-staging \
  req/<date>/<the-object>.json --file /tmp/rec.json && cat /tmp/rec.json
```

It should contain `body`, `headers`, `status`, `outcome`, and a `logs`
array.

> **Do NOT trust `wrangler r2 bucket info` to confirm recording works.**
> Its `object_count` / `bucket_size` are eventually-consistent and lag by
> minutes-to-hours — it will show `0` long after objects have landed. This
> burned a whole debugging session (objects were writing fine the entire
> time). The authoritative check is `r2 object get` / `r2 object list`
> above, plus the synchronous `recording: R2 put OK key=...` line in
> `wrangler tail` — that line is emitted from inside the worker the moment
> the put succeeds, so if you see it, the object exists.

## 6. Record the deploy

Update `DEPLOYED.md` (staging row) with the backend tag (`v23`), the
deploy date (Brisbane), and confirm the `contract` is still `v3`.

---

## Notes

- **The R2 binding is fail-soft.** If you deploy before the bucket exists,
  the worker still trades — it just logs
  `recording: no TRADE_CONTROL_R2 bucket bound — skipped` and records
  nothing. So a missing-bucket mistake costs recording, never a trade.
- **KV id placeholder guard.** If you deploy with
  `id = "REPLACE_ME_STAGING_KV_ID"` still in place, wrangler will error on
  an invalid namespace id — that's the intended tripwire, not a real
  outage. Fix step 1 and redeploy.
