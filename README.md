# slop-proxy

Serve Anthropic- and OpenAI-compatible API endpoints backed by OpenAI Codex
subscription (ChatGPT) accounts and Anthropic (Claude Max) accounts. Log in
through the CLI, pool multiple accounts with rotation and failover, issue
per-user API tokens, and track token usage.

Requests for `claude-*` models (configurable via `models.anthropic_patterns`)
are relayed verbatim to the Anthropic API over the pooled Max accounts, sticky
per session so prompt caches keep hitting. Everything else is translated to
the Codex backend. Log in to Max accounts with
`slop-proxy login --provider anthropic`.

Endpoints: `POST /v1/messages`, `POST /v1/chat/completions`, `GET /v1/models`,
`POST /v1/responses` — streaming, tools, images, and reasoning. Requested model
names pass through to the backend as-is; use `slop-proxy models` for the real slugs.
OpenAI latency variants using the generic `-fast` suffix are also accepted when
an account catalog advertises their base model, such as `gpt-6-astra-fast` when
`gpt-6-astra` is available.

## NixOS module

```nix
{
  inputs.slop-proxy.url = "github:koss/slop-proxy";

  outputs = { nixpkgs, slop-proxy, ... }: {
    nixosConfigurations.host = nixpkgs.lib.nixosSystem {
      modules = [
        slop-proxy.nixosModules.default
        {
          services.slop-proxy = {
            enable = true;
            bind = "[::1]:8484";
          };
        }
      ];
    };
  };
}
```

The service runs as a dedicated `slop-proxy` user with its database at
`/var/lib/slop-proxy/slop.db`. Log in and mint tokens against that database:

```sh
slop-proxy slop-proxy --db /var/lib/slop-proxy/slop.db login
slop-proxy slop-proxy --db /var/lib/slop-proxy/slop.db token create --user alice
```

Point a client at it with the issued token:

```sh
ANTHROPIC_BASE_URL=http://[::1]:8484 ANTHROPIC_API_KEY=sp-... claude
OPENAI_BASE_URL=http://[::1]:8484/v1 OPENAI_API_KEY=sp-...
```

## Gemini keys

Add an unrestricted key with `accounts add-key --provider gemini`. A key
restricted to an HTTP referrer also needs `--referer`.

```sh
slop-proxy accounts add-key \
  --provider gemini \
  --key "$GEMINI_API_KEY" \
  --referer https://conceptcomix.web.app/
```

Referrer-restricted keys use Google's native Gemini surface because its
OpenAI-compatible endpoint drops the referrer before validating the key.

## Experiential keys

Add a gateway key and opt models into its messages endpoint.

```sh
slop-proxy accounts add-key --provider experiential --key "$EXPERIENTIAL_API_KEY"
```

```toml
[models]
experiential_patterns = ["gpt-6-astra"]
```

This integration supports `/v1/messages` only. Experiential models are not
served over `/v1/responses` or `/v1/chat/completions`. Existing provider routes
stay unchanged until you configure `experiential_patterns`.

## Zen egress proxies

Set `zen.proxy_urls` to send only OpenCode Zen traffic through HTTP proxies.
Credentials may be included in each URL. Requests rotate across the configured
egresses. A network failure skips that proxy for 30 seconds, while an anonymous
`429` honors the upstream cooldown and moves the request to another proxy.

```toml
[zen]
proxy_urls = [
  "http://user:password@proxy-one.example:8080",
  "http://user:password@proxy-two.example:8080",
]
```

`zen.proxy_urls_file` reads one URL per line and may be combined with the inline
list. Use the file setting when URLs contain credentials that should stay out
of the config and the Nix store. Configuring either list disables direct Zen
egress.

## Per-token limits and metering

Each issued token can carry rolling-window limits. Omitted request or token
limits are unlimited. Token usage counts input plus output tokens as settled
after each request.

```sh
# 60 requests and 100k tokens per hour, each admitted request delayed 250ms
slop-proxy token create --user alice \
  --requests 60 --tokens 100000 --window-seconds 3600 --slowdown-ms 250

slop-proxy token limits 1 --requests 120 --window-seconds 3600
slop-proxy token usage 1
```

Admissions persist before upstream dispatch, so concurrent requests cannot
race past the request limit. A request that takes the token total over its
limit completes, and later requests get `429` until usage rolls out of the
window. Responses carry `x-ratelimit-*` headers, and limit errors include
`retry-after`.

Codex tokens may also be capped against the provider-reported subscription
windows. Values use percentages; omit either flag to leave that window
unlimited. If Codex does not report a configured window, that check is skipped.

```sh
slop-proxy token create --user alice --5hr-limit 50% --weekly-limit 50%
```

These `--5hr-limit` and `--weekly-limit` flags remain **account-wide utilization
ceilings**, not personal allowances. For example, `--5hr-limit 50%` stops that
token from using an account once the account's reported five-hour utilization
reaches 50%, regardless of who consumed it. Their semantics are unchanged.

## Estimated per-user subscription quota

Quota accounting estimates each user's consumption of a Codex account's main
five-hour and weekly subscription allowances. It groups usage by the token's
`--user` value, across **all tokens for that user**. Issuing another token for
`kader` does not create a separate allowance. Separately metered Spark quota is
not included. Other providers are not supported by this accounting yet.

Reports and budgets can be per account or fleet-wide, and always refer to a
provider subscription window rather than the rolling `--window-seconds` windows
used for request and token limits. With `--account`, a percentage is a share of
that account's allowance. Without `--account`, percentage budgets apply to the
user's fleet: they are a share of the remaining known capacity across shared
accounts at the current provider-epoch snapshots. Capacity and attributed usage
are weighted by subscription plan: Plus is 1x, Prolite is 5x, and Pro is 20x.
For example, 20 remaining points on a Pro account contribute 400 fleet points,
while 20 remaining points on a Plus account contribute 20. A 50% fleet budget
allows half of the resulting weighted capacity. Personal accounts whose
allowlist names exactly one user are excluded from fleet budgets.

```sh
# JSON report across accounts, or for one account
slop-proxy quota usage --user kader
slop-proxy quota usage --user kader --account personal

# Each account selector can be an ID, email, or label. This is account-scoped.
slop-proxy quota budget --user kader --account personal \
  --5hr-budget 25% --weekly-budget 25% --usd-budget 400

# Without --account, percentage budgets apply across the user's whole fleet.
slop-proxy quota budget --user kader --5hr-budget 50% --weekly-budget 50%

# Replace all budgets: retain a five-hour budget and clear the other windows.
slop-proxy quota budget --user kader --account personal --5hr-budget 25%

# Clear both percentage budgets for this user on this account.
slop-proxy quota budget --user kader --account personal

# Clear both fleet-wide percentage budgets.
slop-proxy quota budget --user kader
```

Window budget values must include `%` and be between `0%` and `100%`. The
`--usd-budget` value is a nonnegative dollar amount, such as `400` or `$400`.
USD budgets remain account-scoped, so `--usd-budget` must be used together with
`--account`. Each `quota budget` command replaces both percentage budgets for
the selected target; an omitted budget is cleared (unlimited). For an
account-scoped command, the USD budget is replaced too. No user budgets
exist by default; `kader` stays unlimited unless you explicitly set one.
Existing token limits and provider limits still apply.

Fleet percentage budgets reset when the provider's five-hour or weekly epoch
resets. The proxy reports percentage points against the remaining known fleet
capacity from the current snapshots. Estimates are weighted across accounts,
prefer public model prices when available, and fall back to weighted input,
output, and cache token counts when prices are unavailable. These estimates are
not exact token counts or provider billing figures.

A `25%` budget allows estimated consumption of 25% of the account's full
allowance, stored as **25 percentage points**. A reported estimate of 10 means
10% of the full allowance, or 10 percentage points of utilization. Against a
25% budget, that is 40% of the user's budget, not 10% of that budget. Quota
percentages are not exact token counts or provider billing figures.

The USD budget is an estimate based on public model list prices. It is not
subscription billing, an invoice, or a charge to the user. Unlike the
provider's rolling windows, it lasts for the lifetime of the user/account
policy and must be cleared manually with `--usd-budget` omitted. USD usage is
only reliable when the proxy has a current model-pricing cache; refresh or
restore that cache before relying on this limit. Requests are counted after
they settle, so delayed settlement and in-flight requests can delay
enforcement and can cause overshoot.

JSON reports distinguish the provider's account-wide `observed_used_percent`
from the proxy's per-user `estimated_user_percent` (percentage points of the
full subscription window). `budget_percent` is the assigned share, while
`allowance_used_percent` is the estimated percentage of that share consumed.
The latter is undefined (`null`) without a current reading or with a zero
budget. `baseline_percent` and `unattributed_percent` describe usage that was
not assigned to a user. `estimated_cost_usd` is lifetime settled list-price
value for the user and account. When configured, `spend_budget_usd` is the
lifetime USD ceiling and `spend_allowance_used_percent` is its estimated
share consumed. All reports are marked `estimated: true`.

The proxy polls provider usage about once a minute. It distributes observed
increases among newly settled, non-Spark Codex requests. When all requests in
an interval have model pricing, relative estimated dollar cost supplies the
weights. Otherwise the entire interval uses weighted input, output, and cache
token counts. This accounts for different models and cache usage better than
raw token counts, but does not reproduce the provider's private quota formula.
A provider reading covers the whole account; a user estimate covers work
attributed to that user by this proxy, subject to the limits below.

### Estimation limits

- An empty report means there are no current readings or configured budgets
  for the selected user/accounts. It does not prove exactly zero consumption.
  A budget-only row can have `estimated_user_percent: 0` with null observation
  fields. That is zero in the known ledger, not a measured zero. A window that
  has expired also needs a new reading before its utilization is known.
- Accounting starts with observations made by this proxy. If the account is
  already at 44% when first observed, that 44% is an unattributed baseline. It
  cannot be assigned retroactively to `kader` or any other user.
- The first sample, including the first sample after a new reset, establishes
  an unattributed baseline. Only later observed increases can be attributed.
- Provider samples can be delayed or rounded. Attribution is an estimate,
  not a provider-exact measurement of each user's subscription usage.
- Usage outside the proxy cannot be reliably distinguished from proxy usage
  when they overlap. Such usage can affect the estimated shares.
- Budget checks act on observed estimates. Requests already in flight can
  complete after a budget is reached, so delayed samples, rounding, and
  concurrent requests can cause overshoot. These are not hard prepaid caps.
