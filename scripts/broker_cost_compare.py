"""Cost comparison: OANDA / TradeNation / IBKR futures, 3-day hold, ~$1M notional.

All inputs sourced 2026-07-26/27 (see SCOPING doc for citations).
Convention: cost is what WE PAY on a LONG position, in USD, positive = cost.
"""

DAYS = 3

# --- policy rates (fetched 2026-07-27) ---
FED = 0.03625   # 3.50-3.75% midpoint
RBA = 0.0360
RBNZ = 0.0250   # hiked 8 Jul 2026
BOJ = 0.0100
SORA = 0.0127   # ~1.21-1.33%

# --- admin fees (CORRECTED 2026-07-29) ---
#
# Previously a single flat 2.5% for OANDA and an assumed 3.0% for TN. Both wrong.
#
# OANDA's fee is ASSET-CLASS DEPENDENT, and it is derivable from their own
# published v20 rates rather than assumed: for FX, longRate + shortRate = -2*admin,
# so admin = -(long+short)/2. Measured across EUR/USD, GBP/USD, AUD/USD, USD/JPY
# that lands at 1.01-1.035%; for SPX500/NAS100/UK100 it is exactly 2.500%.
#
# TN publishes 2.5% (not the ~3.0% previously inferred from modelled figures).
#
# Source: scripts/oanda_financing.py against /v3/accounts/{id}/instruments.
OANDA_FX_ADMIN = 0.0102    # measured mean of the four majors
OANDA_CFD_ADMIN = 0.025    # exactly 2.500% — confirmed on indices
TN_ADMIN = 0.025           # published; applies to both FX and CFD

# OANDA's ACTUAL published long financing rates (%/yr, negative = we pay).
# These BEAT the modelled base-minus-quote-plus-admin approach because they
# already fold in OANDA's own carry assumptions, not just policy-rate midpoints.
# Used where available; the model is the fallback for pairs not pulled.
OANDA_MEASURED_LONG = {
    "AUD_NZD": 0.0079,     # +0.79%/yr — a CREDIT on the long side
    "XAU_USD": -0.0521,
    "SPX500": -0.0614,
}

def fx_financing(notional_usd, base_rate, quote_rate, admin, days=DAYS):
    """Long base/quote: earn base, pay quote, pay admin on the lot.

    MODELLED fallback. Prefer measured_financing() when the pair is in
    OANDA_MEASURED_LONG — policy-rate midpoints are an approximation of what
    the broker actually charges.
    """
    net_rate = -(base_rate - quote_rate) + admin   # cost positive
    return notional_usd * net_rate * days / 365

def measured_financing(notional_usd, long_rate_pa, days=DAYS):
    """Cost from a broker's OWN published long rate. Negative rate = we pay."""
    return notional_usd * -long_rate_pa * days / 365

def flat_financing(notional_usd, bench, admin, days=DAYS):
    """Index/commodity CFD long: pay benchmark + admin on full notional."""
    return notional_usd * (bench + admin) * days / 365

print("="*70)
print("1. AUD/NZD  — 1,000,000 AUD notional")
print("="*70)
audnzd = 1.20658
aud_usd = 0.655   # approx; only used to express notional in USD
notional_aud = 1_000_000
notional_usd = notional_aud * aud_usd
print(f"notional: {notional_aud:,.0f} AUD = ${notional_usd:,.0f} USD @ {aud_usd}")

# TN: spread 18 points on AUD/NZD (1.20658). TN quotes 18 = 0.00018
tn_spread_nzd = 0.00018 * notional_aud
nzd_usd = aud_usd / audnzd
tn_spread_usd = tn_spread_nzd * nzd_usd
tn_fin = fx_financing(notional_usd, RBA, RBNZ, TN_ADMIN)
print(f"\nTradeNation: spread {tn_spread_usd:6.0f}  fin {tn_fin:6.0f}  TOTAL {tn_spread_usd+tn_fin:6.0f}")

# OANDA Core: ~1.5 pip typical on AUD/NZD (cross, wider than majors) + commission
oa_spread_usd = 0.00015 * notional_aud * nzd_usd
oa_comm = notional_usd * 0.00005 * 2   # ~$50/M/side Core
oa_fin_modelled = fx_financing(notional_usd, RBA, RBNZ, OANDA_FX_ADMIN)
# MEASURED beats modelled: OANDA publishes AUD_NZD longRate = +0.79%/yr, i.e. a
# CREDIT on the long side. The policy-rate model says we PAY. Use the real number.
oa_fin = measured_financing(notional_usd, OANDA_MEASURED_LONG["AUD_NZD"])
print(f"OANDA Core : spread {oa_spread_usd:6.0f}  comm {oa_comm:5.0f}  fin {oa_fin:6.0f}  TOTAL {oa_spread_usd+oa_comm+oa_fin:6.0f}")
print(f"  (modelled fin would be {oa_fin_modelled:.0f} — measured is {oa_fin_modelled-oa_fin:+.0f} different)")

# OANDA Standard: ~2.8 pip, no comm
oas_spread_usd = 0.00028 * notional_aud * nzd_usd
print(f"OANDA Std  : spread {oas_spread_usd:6.0f}  fin {oa_fin:6.0f}  TOTAL {oas_spread_usd+oa_fin:6.0f}")

# IBKR futures ANE: contract 200,000 AUD, OI only ~225 -> illiquid
ane_contract_aud = 200_000
n_ane = notional_aud / ane_contract_aud
print(f"\nIBKR ANE futures: would need {n_ane:.1f} contracts; TOTAL MARKET OI ~225")
print(f"  -> position = {n_ane/225*100:.1f}% of entire open interest. NOT VIABLE.")

print()
print("="*70)
print("2. SGD/JPY — availability")
print("="*70)
print("TradeNation : NO SGD pairs at all (MCP search 'SGD' -> empty)")
print("CME futures : no SGD/JPY cross contract exists")
print("OANDA       : yes (spot)")
print("IBKR spot   : yes (IDEALPRO)")
sgdjpy_notional = 1_000_000  # SGD
sgd_usd = 0.78
sj_notional_usd = sgdjpy_notional * sgd_usd
sj_fin_oanda = fx_financing(sj_notional_usd, SORA, BOJ, OANDA_FX_ADMIN)
print(f"\nOANDA fin on ${sj_notional_usd:,.0f}: {sj_fin_oanda:.0f} (rate diff SORA {SORA:.2%} - BoJ {BOJ:.2%})")
print("  -> only OANDA can trade it of your two brokers; futures impossible")

print()
print("="*70)
print("3. S&P 500 — ~$1M notional, index ~7468")
print("="*70)
spx = 7468.03
notional = 1_000_000

# TN: US 500 spread 0.5 index points, per 1.0 -> $1 per point per unit
tn_units = notional / spx
tn_spread = 0.5 * tn_units
tn_fin_spx = flat_financing(notional, FED, TN_ADMIN)
print(f"TradeNation: units {tn_units:.1f}  spread {tn_spread:6.0f}  fin {tn_fin_spx:6.0f}  TOTAL {tn_spread+tn_fin_spx:6.0f}")

# OANDA SPX500: ~0.4 pt typical
oa_spread_spx = 0.4 * tn_units
oa_fin_spx = flat_financing(notional, FED, OANDA_CFD_ADMIN)
print(f"OANDA      : spread {oa_spread_spx:6.0f}  fin {oa_fin_spx:6.0f}  TOTAL {oa_spread_spx+oa_fin_spx:6.0f}")

# IBKR ES futures: $50/point, tick 0.25 = $12.50
es_notional = spx * 50
n_es = notional / es_notional
es_spread = n_es * 12.50          # 1 tick crossing
es_comm = n_es * 2.25 * 2         # ~$2.25/contract/side all-in
# futures financing = basis (embedded). Implied cost ~ risk-free on notional, NO admin fee
es_fin = notional * FED * DAYS / 365
print(f"IBKR ES    : {n_es:.2f} contracts  spread {es_spread:6.0f}  comm {es_comm:5.0f}  basis {es_fin:6.0f}  TOTAL {es_spread+es_comm+es_fin:6.0f}")

print()
print("="*70)
print("4. GOLD — ~$1M notional, spot ~4089")
print("="*70)
gold = 4089.46
oz = notional / gold
print(f"notional ${notional:,} = {oz:.1f} oz")

# TN Spot Gold: spread 6.0 -> $0.60? TN quotes gold spread in cents. 6.0 = $0.60/oz
tn_gold_spread = 0.60 * oz
tn_gold_fin = flat_financing(notional, FED, TN_ADMIN)
print(f"TradeNation: spread {tn_gold_spread:6.0f}  fin {tn_gold_fin:6.0f}  TOTAL {tn_gold_spread+tn_gold_fin:6.0f}")

# OANDA XAU/USD: ~$0.35/oz typical
oa_gold_spread = 0.35 * oz
oa_gold_fin = flat_financing(notional, FED, OANDA_CFD_ADMIN)
print(f"OANDA      : spread {oa_gold_spread:6.0f}  fin {oa_gold_fin:6.0f}  TOTAL {oa_gold_spread+oa_gold_fin:6.0f}")

# IBKR GC futures: 100 oz, tick 0.10 = $10
gc_notional = gold * 100
n_gc = notional / gc_notional
gc_spread = n_gc * 10.0    # 1 tick
gc_comm = n_gc * 2.50 * 2
gc_fin = notional * FED * DAYS / 365   # embedded basis, no admin
print(f"IBKR GC    : {n_gc:.2f} contracts  spread {gc_spread:6.0f}  comm {gc_comm:5.0f}  basis {gc_fin:6.0f}  TOTAL {gc_spread+gc_comm+gc_fin:6.0f}")
