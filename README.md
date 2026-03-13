# Trade Control Web Hook

Cloudflare Worker that receives TradingView alerts and controls open OANDA trades.

Currently supports closing all positions for an instrument.

## Payload format

Send a POST request with a YAML body:

```yaml
token: <your AUTH_TOKEN>
instrument: EUR_USD
action: close
```

Use the OANDA instrument name (e.g. `EUR_USD`, `GBP_HKD`, `SPX500_USD`). In TradingView alerts, `{{ticker}}` works for forex pairs but for indices you should hardcode the OANDA name.

## Test locally

```sh
cp dev.vars.example .dev.vars
# edit .dev.vars with your secrets
wrangler dev
```

Then send a test request:

```sh
http POST localhost:8787 Content-Type:text/plain --raw 'token: <AUTH_TOKEN>
instrument: EUR_USD
action: close'
```

## Deploy

### First deploy — push all secrets

```sh
wrangler secret put AUTH_TOKEN
wrangler secret put OANDA_API_KEY
wrangler secret put OANDA_ACCOUNT_ID
wrangler secret put OANDA_LIVE
wrangler deploy
```

Set `OANDA_LIVE` to `true` for live trading, `false` for practice.

### Subsequent deploys

```sh
wrangler deploy
```
