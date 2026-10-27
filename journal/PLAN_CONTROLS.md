# Plan controls

Open a plan with `Enter` or `→` in `journal-staging`. The row below the plan's
information bar has three buttons. Click them or use the indicated keys.

| Control | Key | Result |
| --- | --- | --- |
| Paper mode | `p` | Choose another account for market data and log paper entries without broker execution. |
| Swap account | `b` | Choose another configured account for future execution and market data. The current broker / paper mode is preserved. |
| Closed manually | `m` | Record that you already closed the position and cancelled pending orders, then stop plan management. |

The account picker reads `trade-control-accounts list --json`; rebuild that
binary alongside `journal-staging` and `trade-control-staging`. It shows each
account's name, broker and demo/live kind, excluding the plan's current account.
Choose with `↑`/`↓` and `Enter`, or click an account. Account credentials are
not checked by the picker; choose an account with working market data.

Every change has a confirmation with `y` / Confirm and `n` / Esc / Cancel.
Switching accounts or entering paper mode detaches existing broker
positions and pending orders from plan management. Those positions and orders
stay at the old broker; close or cancel them separately if necessary.

Paper mode logs entry signals. It does not simulate fills or P&L.
An account marked `demo` still uses that broker's demo execution; the dedicated
paper mode control is what prevents broker orders and broker management.

Manual closure is accepted immediately as an operator assertion. The button sends
no broker close or cancel request. A best-effort history audit reads the broker's
settlement / closed-trade ledger afterward, with a 30-minute retry cooldown;
expired credentials do not block the closure. All three actions remain
in the plan timeline for later review.
On success, the journal reloads the plan's timeline, details and list row.
Failures appear in the status line. If a request times out, reload the plan to
check whether the server applied it before trying again.
