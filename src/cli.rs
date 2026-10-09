use std::path::PathBuf;

use eyre::{Result, bail, eyre};
use pound::Parse;

use crate::clock;
use crate::codex::client::CodexClient;
use crate::codex::models::ModelInfo;
use crate::config::Config;
use crate::db::Db;
use crate::db::accounts::NewAccount;
use crate::db::accounts::{Account, AccountField, AccountStatus};
use crate::db::tokens;
use crate::db::tokens::TokenLimits;
use crate::oauth;
use crate::oauth::anthropic;
use crate::oauth::copilot;
use crate::oauth::glm;
use crate::pool::codex::CodexPool;
use crate::provider::{AuthMode, Provider};
use crate::server;
use crate::stats;

/// Anthropic/OpenAI API proxy backed by pooled provider accounts
#[derive(Parse)]
#[pound(name = "slop-proxy")]
pub struct Cli {
   /// Path to the sqlite database
   #[pound(long, global)]
   pub db: Option<PathBuf>,

   /// Path to config.toml
   #[pound(long, global)]
   pub config: Option<PathBuf>,

   #[pound(short, long, global)]
   pub verbose: bool,

   #[pound(subcommand)]
   pub command: Command,
}

#[derive(Parse)]
pub enum Command {
   /// Log in to a subscription account and store it
   Login {
      /// Human-readable label for the account
      #[pound(long)]
      label: Option<String>,

      /// Which backend to log in to
      #[pound(long, default = "openai")]
      provider: Provider,
   },
   /// Manage stored accounts
   Accounts {
      #[pound(subcommand)]
      command: AccountsCommand,
   },
   /// Manage issued API tokens
   Token {
      #[pound(subcommand)]
      command: TokenCommand,
   },
   /// Report and limit estimated per-user Codex subscription usage
   Quota {
      #[pound(subcommand)]
      command: QuotaCommand,
   },
   /// Run the API server
   Serve {
      /// Listen address
      #[pound(long)]
      bind: Option<String>,
   },
   /// Show usage statistics as JSON
   Stats {
      /// Window start: 24h, 7d, 30m, or RFC3339
      #[pound(long)]
      since: Option<String>,
      /// Window end (RFC3339)
      #[pound(long)]
      until: Option<String>,
   },
   /// List the models available from the codex backend, as JSON
   Models,
}

#[derive(Parse)]
pub enum AccountsCommand {
   /// List stored accounts
   List,
   /// Store an account that authenticates with a long-lived API key
   AddKey {
      #[pound(long)]
      provider: Provider,
      #[pound(long)]
      key: String,
      #[pound(long)]
      label: Option<String>,
      #[pound(long)]
      referer: Option<String>,
      /// Send this account through the configured egress proxies
      #[pound(long)]
      egress: bool,
   },
   /// Remove an account by id or email
   Remove { account: String },
   /// Mark an account as trusted, or clear the flag with --off
   Trust {
      account: String,
      #[pound(long)]
      off: bool,
   },
   /// Reserve an account for reserved-only tokens, or release it with --off
   Reserve {
      account: String,
      #[pound(long)]
      off: bool,
   },
   /// Send this account through the configured egress proxies, or direct with --off
   Egress {
      account: String,
      #[pound(long)]
      off: bool,
   },
   /// Restrict an account to these users, comma separated. Omit to allow all.
   Users {
      account: String,
      #[pound(long)]
      allow: Option<String>,
   },
   /// Take an account out of service until `enable`
   Disable {
      account: String,
      #[pound(long)]
      reason: Option<String>,
   },
   /// Return a disabled account to service
   Enable { account: String },
}

#[derive(Parse)]
pub enum TokenCommand {
   /// Issue a new API token for a user
   Create {
      #[pound(long)]
      user: String,
      /// Maximum requests in each rolling window
      #[pound(long, min = "1")]
      requests: Option<i64>,
      /// Maximum input plus output tokens in each rolling window
      #[pound(long, min = "1")]
      tokens: Option<i64>,
      /// Maximum Codex five-hour quota utilization, such as 50%
      #[pound(long = "5hr-limit")]
      five_hour_limit: Option<String>,
      /// Maximum Codex weekly quota utilization, such as 50%
      #[pound(long)]
      weekly_limit: Option<String>,
      #[pound(long, default = "3600", min = "1")]
      window_seconds: i64,
      /// Delay every admitted request by this many milliseconds
      #[pound(long, default = "0", min = "0")]
      slowdown_ms: i64,
      /// Serve this token from trusted accounts when any are available
      #[pound(long)]
      prefer_trusted: bool,
      /// Serve this token only from reserved accounts
      #[pound(long)]
      reserved_only: bool,
      /// Providers this token may reach, comma separated. Empty allows all.
      #[pound(long)]
      providers: Option<String>,
      /// Serve this token only from the named account, by id, email or label
      #[pound(long)]
      pin_account: Option<String>,
   },
   /// List issued tokens
   List,
   /// Revoke a token by id or prefix
   Revoke { token: String },
   /// Replace limits for a token id or prefix; omitted limits are unlimited
   Limits {
      token: String,
      #[pound(long, min = "1")]
      requests: Option<i64>,
      #[pound(long, min = "1")]
      tokens: Option<i64>,
      /// Maximum Codex five-hour quota utilization, such as 50%
      #[pound(long = "5hr-limit")]
      five_hour_limit: Option<String>,
      /// Maximum Codex weekly quota utilization, such as 50%
      #[pound(long)]
      weekly_limit: Option<String>,
      #[pound(long, default = "3600", min = "1")]
      window_seconds: i64,
      #[pound(long, default = "0", min = "0")]
      slowdown_ms: i64,
      #[pound(long)]
      prefer_trusted: bool,
      /// Serve this token only from reserved accounts
      #[pound(long)]
      reserved_only: bool,
      /// Providers this token may reach, comma separated. Empty allows all.
      #[pound(long)]
      providers: Option<String>,
      /// Serve this token only from the named account, by id, email or label
      #[pound(long)]
      pin_account: Option<String>,
   },
   /// Show metered usage for a token's current rolling window
   Usage { token: String },
}

#[derive(Parse)]
pub enum QuotaCommand {
   /// Show estimated usage per account and subscription window as JSON
   Usage {
      #[pound(long)]
      user: String,
      #[pound(long)]
      account: Option<String>,
   },
   /// Show configured fleet budgets and remaining allowance
   Fleet,
   /// Show provider-reported quota usage for all accounts
   Accounts,
   /// Replace user quota budgets
   Budget {
      #[pound(long)]
      user: String,
      #[pound(long)]
      account: Option<String>,
      #[pound(long = "5hr-budget")]
      five_hour_budget: Option<String>,
      #[pound(long)]
      weekly_budget: Option<String>,
      #[pound(long)]
      usd_budget: Option<String>,
   },
}

pub async fn run(args: Cli, cfg: Config) -> Result<()> {
   let db = Db::open(&cfg.db_path)?;

   match args.command {
      Command::Login { label, provider } => match provider {
         Provider::OpenAi => oauth::login(&db, label).await,
         Provider::Anthropic => anthropic::login(&db, label).await,
         Provider::Copilot => copilot::login(&db, label).await,
         Provider::Gemini | Provider::DeepSeek | Provider::Experiential | Provider::Zen => {
            bail!("{provider} has no login flow, use `accounts add-key --provider {provider}`")
         },
         Provider::Glm => {
            let key = glm::login().await?;
            accounts_add_key(&db, Provider::Glm, &key, label.as_deref(), None, false).await
         },
      },
      Command::Accounts { command } => match command {
         AccountsCommand::List => accounts_list(&db).await,
         AccountsCommand::AddKey {
            provider,
            key,
            label,
            referer,
            egress,
         } => {
            accounts_add_key(
               &db,
               provider,
               &key,
               label.as_deref(),
               referer.as_deref(),
               egress,
            )
            .await
         },
         AccountsCommand::Remove { account } => accounts_remove(&db, &account).await,
         AccountsCommand::Trust { account, off } => {
            accounts_toggle(
               &db,
               &account,
               AccountField::Trusted,
               !off,
               ["trusted", "untrusted"],
            )
            .await
         },
         AccountsCommand::Reserve { account, off } => {
            accounts_toggle(
               &db,
               &account,
               AccountField::Reserved,
               !off,
               ["reserved", "unreserved"],
            )
            .await
         },
         AccountsCommand::Egress { account, off } => {
            accounts_toggle(
               &db,
               &account,
               AccountField::Egress,
               !off,
               ["egressed", "direct"],
            )
            .await
         },
         AccountsCommand::Users { account, allow } => {
            accounts_users(&db, &account, allow.as_deref().unwrap_or_default()).await
         },
         AccountsCommand::Disable { account, reason } => {
            accounts_status(&db, &account, AccountStatus::Disabled, reason.as_deref()).await
         },
         AccountsCommand::Enable { account } => {
            accounts_status(&db, &account, AccountStatus::Active, None).await
         },
      },
      Command::Token { command } => match command {
         TokenCommand::Create {
            user,
            requests,
            tokens,
            five_hour_limit,
            weekly_limit,
            window_seconds,
            slowdown_ms,
            prefer_trusted,
            reserved_only,
            providers,
            pin_account,
         } => {
            let limits = token_limits(
               requests,
               tokens,
               five_hour_limit,
               weekly_limit,
               window_seconds,
               slowdown_ms,
               prefer_trusted,
               reserved_only,
               providers,
               resolve_pin(&db, pin_account).await?,
            )?;
            token_create(&db, &user, &limits).await
         },
         TokenCommand::List => token_list(&db).await,
         TokenCommand::Revoke { token } => token_revoke(&db, &token).await,
         TokenCommand::Limits {
            token,
            requests,
            tokens,
            five_hour_limit,
            weekly_limit,
            window_seconds,
            slowdown_ms,
            prefer_trusted,
            reserved_only,
            providers,
            pin_account,
         } => {
            let limits = token_limits(
               requests,
               tokens,
               five_hour_limit,
               weekly_limit,
               window_seconds,
               slowdown_ms,
               prefer_trusted,
               reserved_only,
               providers,
               resolve_pin(&db, pin_account).await?,
            )?;
            token_set_limits(&db, &token, &limits).await
         },
         TokenCommand::Usage { token } => token_usage(&db, &token).await,
      },
      Command::Quota { command } => {
         println!("{}", quota_command(&db, &cfg, command).await?);
         Ok(())
      },
      Command::Serve { bind } => {
         let bind = bind.unwrap_or_else(|| cfg.bind.clone());
         server::serve(db, cfg, &bind).await
      },
      Command::Stats { since, until } => stats::run(&db, since, until).await,
      Command::Models => models(&db, &cfg).await,
   }
}

/// A key is its own identity here. Google exposes nothing to call for an
/// account id, and hashing the key keeps re-adding the same one an update
/// rather than a duplicate slot.
async fn accounts_add_key(
   db: &Db,
   provider: Provider,
   key: &str,
   label: Option<&str>,
   referer: Option<&str>,
   egress: bool,
) -> Result<()> {
   if referer.is_some() && provider != Provider::Gemini {
      bail!("--referer is only supported for gemini keys");
   }
   let (account_id, email, refresh_token, auth_mode, token) = if provider == Provider::Copilot {
      let token = key.trim();
      let login = copilot::github_login(token).await?;
      (login.clone(), Some(login), token, AuthMode::OAuth, token)
   } else {
      let mut hasher = hmac_sha256::Hash::new();
      hasher.update(key.as_bytes());
      let hash = data_encoding::HEXLOWER.encode(&hasher.finalize()[..8]);
      (hash, None, "", AuthMode::ApiKey, key)
   };
   let tokens = oauth::TokenSet {
      access_token: token.to_owned(),
      refresh_token: refresh_token.to_owned(),
      id_token: None,
      expires_at: None,
   };
   let id = db
      .upsert_account(NewAccount {
         provider,
         id: &account_id,
         email: email.as_deref(),
         label,
         plan: None,
         tokens: &tokens,
         auth_mode,
      })
      .await?;
   if let Some(referer) = referer {
      let referer = (!referer.is_empty()).then(|| referer.to_owned());
      db.set_account(id, AccountField::HttpReferer(referer))
         .await?;
   }
   if egress {
      db.set_account(id, AccountField::Egress(true)).await?;
   }
   println!("stored {provider} account {id} ({account_id})");
   Ok(())
}

async fn accounts_list(db: &Db) -> Result<()> {
   // A struct keeps field order; serde_json alphabetizes json! maps.
   #[derive(serde::Serialize)]
   struct AccountRow<'a> {
      id: i64,
      provider: &'a str,
      trusted: bool,
      reserved: bool,
      egress: bool,
      email: Option<&'a str>,
      plan_type: Option<&'a str>,
      status: &'static str,
      label: Option<&'a str>,
      cooldown_seconds_left: Option<i64>,
      disabled_reason: Option<&'a str>,
      http_referer: Option<&'a str>,
   }

   let accounts = db.list_accounts().await?;
   let now = clock::unix_now();
   let rows = accounts
      .iter()
      .map(|account| AccountRow {
         id: account.id,
         provider: account.provider.as_str(),
         trusted: account.trusted,
         reserved: account.reserved,
         egress: account.egress,
         email: account.email.as_deref(),
         plan_type: account.plan_type.as_deref(),
         status: account.status.as_str(),
         label: account.label.as_deref(),
         cooldown_seconds_left: (account.status == AccountStatus::Cooldown)
            .then(|| account.cooldown_until.map(|until| (until - now).max(0)))
            .flatten(),
         disabled_reason: account.disabled_reason.as_deref(),
         http_referer: account.http_referer.as_deref(),
      })
      .collect::<Vec<AccountRow>>();
   println!("{}", serde_json::to_string_pretty(&rows)?);
   Ok(())
}

async fn lookup(db: &Db, key: &str) -> Result<Account> {
   db.find_account(key)
      .await?
      .ok_or_else(|| eyre!("no account matched {key:?}"))
}

async fn accounts_toggle(
   db: &Db,
   key: &str,
   field: fn(bool) -> AccountField,
   enabled: bool,
   words: [&str; 2],
) -> Result<()> {
   let found = lookup(db, key).await?;
   db.set_account(found.id, field(enabled)).await?;
   println!("account {key} is now {}", words[usize::from(!enabled)]);
   Ok(())
}

async fn accounts_status(
   db: &Db,
   account: &str,
   status: AccountStatus,
   reason: Option<&str>,
) -> Result<()> {
   let found = lookup(db, account).await?;
   db.set_account_status(found.id, status, None, reason)
      .await?;
   println!("account {account} is now {}", status.as_str());
   Ok(())
}

async fn accounts_users(db: &Db, account: &str, allow: &str) -> Result<()> {
   let users: Vec<&str> = allow
      .split(',')
      .map(str::trim)
      .filter(|user| !user.is_empty())
      .collect();
   let found = lookup(db, account).await?;
   db.set_account(found.id, AccountField::AllowedUsers(users.join(",")))
      .await?;
   if users.is_empty() {
      println!("account {account} is now open to every user");
   } else {
      println!("account {account} now serves only {}", users.join(", "));
   }
   Ok(())
}

async fn accounts_remove(db: &Db, account: &str) -> Result<()> {
   let found = lookup(db, account).await?;
   let count = db.remove_account(found.id).await?;
   println!("removed {count} account(s)");
   Ok(())
}

async fn token_create(db: &Db, user: &str, limits: &TokenLimits) -> Result<()> {
   let (raw, prefix) = tokens::generate();
   let id = db.create_token(user, &raw, &prefix).await?;
   db.set_token_limits(&id.to_string(), limits).await?;
   println!("token for {user}: {raw}");
   println!("(shown once; only a hash is stored)");
   Ok(())
}

/// A pin is stored by id, so a label that no longer resolves must fail loudly
/// rather than quietly leaving the token free to use the whole pool.
async fn resolve_pin(db: &Db, account: Option<String>) -> Result<Option<i64>> {
   let Some(key) = account else {
      return Ok(None);
   };
   Ok(Some(lookup(db, &key).await?.id))
}

#[expect(
   clippy::too_many_arguments,
   reason = "both token subcommands hand over the same set of flags"
)]
fn token_limits(
   requests: Option<i64>,
   tokens: Option<i64>,
   five_hour_limit: Option<String>,
   weekly_limit: Option<String>,
   window_seconds: i64,
   slowdown_ms: i64,
   prefer_trusted: bool,
   reserved_only: bool,
   providers: Option<String>,
   pinned_account: Option<i64>,
) -> Result<TokenLimits> {
   let providers = providers
      .filter(|csv| !csv.trim().is_empty())
      .map(|raw| {
         raw.split(',')
            .map(|part| part.parse::<Provider>().map_err(eyre::Report::msg))
            .collect::<Result<Vec<_>>>()
      })
      .transpose()?
      .unwrap_or_default();
   let five_hour_limit = parse_percentage(five_hour_limit, "--5hr-limit")?;
   let weekly_limit = parse_percentage(weekly_limit, "--weekly-limit")?;
   Ok(TokenLimits {
      requests,
      tokens,
      window_seconds,
      slowdown_ms,
      five_hour_limit,
      weekly_limit,
      prefer_trusted,
      reserved_only,
      pinned_account,
      providers,
   })
}

fn parse_percentage(raw: Option<String>, flag: &str) -> Result<Option<f64>> {
   let Some(raw) = raw else {
      return Ok(None);
   };
   let value = raw
      .strip_suffix('%')
      .ok_or_else(|| eyre!("{flag} must be a percentage such as 50%"))?
      .parse::<f64>()
      .map_err(|_| eyre!("{flag} must be a percentage such as 50%"))?;
   if !value.is_finite() || !(0.0..=100.0).contains(&value) {
      bail!("{flag} must be between 0% and 100%");
   }
   Ok(Some(value / 100.0))
}

async fn quota_command(db: &Db, cfg: &Config, command: QuotaCommand) -> Result<String> {
   match command {
      QuotaCommand::Fleet => quota_fleet(db).await,
      QuotaCommand::Accounts => quota_accounts(db, cfg).await,
      QuotaCommand::Usage { user, account } => {
         let account_id = resolve_pin(db, account).await?;
         let usage = db.user_quota(&user, account_id).await?;
         Ok(serde_json::to_string_pretty(&usage)?)
      },
      QuotaCommand::Budget {
         user,
         account,
         five_hour_budget,
         weekly_budget,
         usd_budget,
      } => {
         // Validate every value before changing any budget. USD budgets are
         // account-scoped, while percentage budgets can target the whole fleet.
         let (five_hour_budget, weekly_budget) = quota_budgets(five_hour_budget, weekly_budget)?;
         let usd_budget = parse_usd_budget(usd_budget, "--usd-budget")?;
         if account.is_none() && usd_budget.is_some() {
            bail!("--usd-budget requires --account (USD budgets are account-scoped)");
         }
         let Some(account) = account else {
            db.set_user_fleet_quota_budgets(&user, five_hour_budget, weekly_budget)
               .await?;
            return Ok(format!("updated fleet quota budgets for {user}"));
         };
         let account_id = resolve_pin(db, Some(account))
            .await?
            .ok_or_else(|| eyre!("--account is required"))?;
         // The quota replacement is atomic for its two windows. The spend
         // budget has its own atomic update; validation above prevents a bad
         // value from leaving either policy half changed.
         db.set_user_quota_budgets(&user, account_id, five_hour_budget, weekly_budget)
            .await?;
         db.set_user_spend_budget(&user, account_id, usd_budget)
            .await?;
         Ok(format!(
            "updated quota budgets for {user} on account {account_id}"
         ))
      },
   }
}

fn compact_number(value: f64) -> String {
   let formatted = format!("{value:.2}");
   formatted
      .trim_end_matches('0')
      .trim_end_matches('.')
      .to_owned()
}

fn quota_reset_in(resets_at: Option<i64>, now: i64) -> String {
   let Some(resets_at) = resets_at else {
      return "-".into();
   };
   let seconds = resets_at.saturating_sub(now).max(0);
   let days = seconds / 86_400;
   let hours = seconds % 86_400 / 3_600;
   let minutes = seconds % 3_600 / 60;
   if days > 0 {
      format!("{days}d {hours}h")
   } else if hours > 0 {
      format!("{hours}h {minutes}m")
   } else {
      format!("{minutes}m")
   }
}

async fn quota_fleet(db: &Db) -> Result<String> {
   let rows = db.fleet_quotas().await?;
   let mut out = String::from(
      "USER              WINDOW  BUDGET  ALLOWANCE  USED       USED%    LEFT       LEFT%    RESETS IN
",
   );
   out.push_str(
      "----------------  ------  ------  ---------  ---------  -------  ---------  -------  ---------
",
   );
   let now = clock::unix_now();
   for row in rows {
      let Some(budget) = row.budget_percent else {
         continue;
      };
      let capacity = row.fleet_capacity_points.unwrap_or(0.0_f64);
      let used = row.fleet_estimated_user_percent.unwrap_or(0.0_f64);
      let allowance = capacity * budget / 100.0_f64;
      let left = (allowance - used).max(0.0_f64);
      let (used_percent, left_percent) = row.fleet_allowance_used_percent.map_or_else(
         || ("-".into(), "-".into()),
         |percent| {
            (
               format!("{}%", compact_number(percent)),
               format!("{}%", compact_number((100.0_f64 - percent).max(0.0_f64))),
            )
         },
      );
      let window = match row.window_seconds {
         18_000 => "5H",
         604_800 => "WEEKLY",
         _ => "OTHER",
      };
      out.push_str(&format!(
         "{:<16}  {:<6}  {:>6}  {:>9}  {:>9}  {:>7}  {:>9}  {:>7}  {}
",
         row.user,
         window,
         format!("{}%", compact_number(budget)),
         compact_number(allowance),
         compact_number(used),
         used_percent,
         compact_number(left),
         left_percent,
         quota_reset_in(row.resets_at, now),
      ));
   }
   out.push_str(
      "
Weighted points: Plus 1x, Prolite 5x, Pro 20x. Personal accounts are excluded.",
   );
   Ok(out)
}

async fn quota_accounts(db: &Db, cfg: &Config) -> Result<String> {
   let client = CodexClient::new(cfg.codex.clone());
   let accounts = db
      .list_accounts()
      .await?
      .into_iter()
      .filter(|account| account.provider == Provider::OpenAi)
      .collect::<Vec<_>>();
   let mut out =
      String::from("ID  ACCOUNT                              PLAN  5H       WEEK     STATUS\n");
   out.push_str(
      "--  ----------------------------------  ----  -------  -------  ----------------\n",
   );
   for account in accounts {
      let display = account
         .email
         .as_deref()
         .or(account.label.as_deref())
         .unwrap_or(&account.provider_account_id);
      let (five_hour, weekly, status) = match client
         .usage(&account.access_token, &account.provider_account_id)
         .await
      {
         Ok(usage) => {
            let mut five_hour = "-".to_owned();
            let mut weekly = "-".to_owned();
            for window in usage.rate_limit.windows() {
               let value = format!("{:.0}%", window.used_percent);
               match window.limit_window_seconds {
                  18_000 => five_hour = value,
                  604_800 => weekly = value,
                  _ => {},
               }
            }
            let status = if usage.rate_limit.limit_reached {
               "LIMIT REACHED"
            } else {
               "available"
            };
            (five_hour, weekly, status.to_owned())
         },
         Err(error) => ("-".into(), "-".into(), format!("error: {error}")),
      };
      out.push_str(&format!(
         "{:<3} {:<36}  {:<4}  {:<7}  {:<7}  {}\n",
         account.id,
         display.chars().take(36).collect::<String>(),
         account.plan_type.as_deref().unwrap_or("-"),
         five_hour,
         weekly,
         status,
      ));
   }
   Ok(out.trim_end().to_owned())
}

fn parse_usd_budget(raw: Option<String>, flag: &str) -> Result<Option<f64>> {
   let Some(raw) = raw else {
      return Ok(None);
   };
   let value = raw
      .strip_prefix('$')
      .unwrap_or(&raw)
      .parse::<f64>()
      .map_err(|_| eyre!("{flag} must be a nonnegative dollar amount such as 400"))?;
   if !value.is_finite() || value < 0.0_f64 {
      bail!("{flag} must be a finite, nonnegative dollar amount");
   }
   Ok(Some(value))
}

fn quota_budgets(
   five_hour_budget: Option<String>,
   weekly_budget: Option<String>,
) -> Result<(Option<f64>, Option<f64>)> {
   // Token ceilings use fractions, but user quota accounting uses percentage points.
   let five_hour =
      parse_percentage(five_hour_budget, "--5hr-budget")?.map(|fraction| fraction * 100.0_f64);
   let weekly =
      parse_percentage(weekly_budget, "--weekly-budget")?.map(|fraction| fraction * 100.0_f64);
   Ok((five_hour, weekly))
}

async fn token_set_limits(db: &Db, token: &str, limits: &TokenLimits) -> Result<()> {
   if db.set_token_limits(token, limits).await? == 0 {
      bail!("no token matched {token:?}");
   }
   println!("updated limits for {token}");
   Ok(())
}

async fn token_usage(db: &Db, token: &str) -> Result<()> {
   let usage = db
      .token_meter(token)
      .await?
      .ok_or_else(|| eyre!("no token matched {token:?}"))?;
   println!("{}", serde_json::to_string_pretty(&usage)?);
   Ok(())
}

async fn token_list(db: &Db) -> Result<()> {
   #[derive(serde::Serialize)]
   struct TokenRow<'a> {
      id: i64,
      user: &'a str,
      prefix: &'a str,
      created_at: i64,
      revoked: bool,
      revoked_at: Option<i64>,
      request_limit: Option<i64>,
      token_limit: Option<i64>,
      five_hour_limit: Option<f64>,
      weekly_limit: Option<f64>,
      window_seconds: i64,
      slowdown_ms: i64,
   }

   let tokens = db.list_tokens().await?;
   let rows = tokens
      .iter()
      .map(|token| TokenRow {
         id: token.id,
         user: &token.user,
         prefix: &token.token_prefix,
         created_at: token.created_at,
         revoked: token.revoked_at.is_some(),
         revoked_at: token.revoked_at,
         request_limit: token.limits.requests,
         token_limit: token.limits.tokens,
         five_hour_limit: token.limits.five_hour_limit,
         weekly_limit: token.limits.weekly_limit,
         window_seconds: token.limits.window_seconds,
         slowdown_ms: token.limits.slowdown_ms,
      })
      .collect::<Vec<TokenRow>>();
   println!("{}", serde_json::to_string_pretty(&rows)?);
   Ok(())
}

async fn token_revoke(db: &Db, token: &str) -> Result<()> {
   let count = db.revoke_token(token).await?;
   if count == 0 {
      bail!("no active token matched {token:?}");
   }
   println!("revoked {count} token(s)");
   Ok(())
}

async fn models(db: &Db, cfg: &Config) -> Result<()> {
   #[derive(serde::Serialize)]
   struct ModelRow<'a> {
      #[serde(flatten)]
      info: &'a ModelInfo,
      listed: bool,
   }

   let client = CodexClient::new(cfg.codex.clone());
   let pool = CodexPool::load(db.clone(), client).await?;

   let models = match pool.list_models().await {
      Ok(models) => models,
      Err(err) => {
         eprintln!("could not fetch models from the codex backend: {err}");
         Vec::new()
      },
   };

   let arr = models
      .iter()
      .map(|model| ModelRow {
         info: model,
         listed: model.listed(),
      })
      .collect::<Vec<_>>();
   println!("{}", serde_json::to_string_pretty(&arr)?);
   Ok(())
}
