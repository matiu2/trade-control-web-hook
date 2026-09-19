# REBLESS 2026-09-19 — repair of a stale-binary re-bless (192 goldens)

Commit `fb1d808e` ("rebless") overwrote 192 `expected.json` files with the
output of a binary older than the source it was committed on. Evidence: at
`fb1d808e` the source itself (and the `replay-candles-staging` binary installed
from it, `v147-51-gfb1d808e`) reproduces the goldens of `38136da4`, not the ones
`fb1d808e` wrote — e.g. `aud-cad-h1-2026-07-28-normal-news-off` is one leg
+1.98R from source, but was blessed as "no legs".

This commit re-blesses exactly those 192 cells with a binary built from this
tree. No other golden moved. It is the third stale-binary re-bless
(`97ac98f1`, then this); check `replay-candles-staging --version` against
`git log -1` before blessing.

Unrelated to the session-anchor work it sits next to: every one of the 192 is
an FX cell, and FX keeps the 17:00 New York grid.
