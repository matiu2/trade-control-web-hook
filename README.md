# Cloud flare worker

Allows control of ongoing oanda trades from trading view web hooks.

Initially, it'll just allow me to close all positions on an instrument.

Use `worker-build` for building. (Builds to wasm to be a cloudflare worker).
use `cargo test` for a native test

## Test locally

```sh
cp dev.vars.example .dev.vars
{edit .dev.vars}
wrangler dev
```

## First deploy

```shell
$ wrangler secret put AUTH_TOKEN


 ⛅️ wrangler 4.72.0
───────────────────
Attempting to login via OAuth...
Opening a link in your default browser: ...
Successfully logged in.
✔ Enter a secret value: … ***
🌀 Creating the secret for the Worker "phone-web-hook" 
✔ There doesn't seem to be a Worker called "phone-web-hook". Do you want to create a new Worker with that name and add secrets to it? … yes
🌀 Creating new Worker "phone-web-hook"...
✨ Success! Uploaded secret SECRET_TOKEN
```

*repeat for all secrets in `dev.vars`*

## Subsequent deploys

```sh
wrangler deploy
```
