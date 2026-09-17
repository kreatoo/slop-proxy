use std::path::PathBuf;

use eyre::{Result, bail, eyre};
use pound::Parse;

use crate::clock;
use crate::codex;
use crate::codex::client::CodexClient;
use crate::codex::models::ModelInfo;
use crate::config::Config;
use crate::db::Db;
use crate::db::accounts::AccountStatus;
use crate::db::accounts::NewAccount;
use crate::db::tokens;
use crate::db::tokens::TokenLimits;
use crate::oauth;
use crate::oauth::anthropic;
use crate::oauth::refresh;
use crate::pool::codex::CodexPool;
use crate::provider::{AuthMode, Provider};
use crate::server;
use crate::stats;

/// Anthropic/OpenAI API proxy backed by Codex subscription accounts
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
   /// Debug helpers
   #[pound(hidden)]
   Debug {
      #[pound(subcommand)]
      command: DebugCommand,
   },
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
   },
   /// Remove an account by id or email
   Remove { account: String },
   /// Mark an account as trusted, or clear the flag with --off
   Trust {
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
}

#[derive(Parse)]
pub enum TokenCommand {
   /// Issue a new API token for a user
   Create {
      #[pound(long)]
      user: String,
      /// Maximum requests in each rolling window
      #[pound(long)]
      requests: Option<i64>,
      /// Maximum input plus output tokens in each rolling window
      #[pound(long)]
      tokens: Option<i64>,
      /// Maximum Codex five-hour quota utilization, such as 50%
      #[pound(long = "5hr-limit")]
      five_hour_limit: Option<String>,
      /// Maximum Codex weekly quota utilization, such as 50%
      #[pound(long)]
      weekly_limit: Option<String>,
      #[pound(long, default = "3600")]
      window_seconds: i64,
      /// Delay every admitted request by this many milliseconds
      #[pound(long, default = "0")]
      slowdown_ms: i64,
      /// Serve this token from trusted accounts when any are available
      #[pound(long)]
      prefer_trusted: bool,
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
      #[pound(long)]
      requests: Option<i64>,
      #[pound(long)]
      tokens: Option<i64>,
      /// Maximum Codex five-hour quota utilization, such as 50%
      #[pound(long = "5hr-limit")]
      five_hour_limit: Option<String>,
      /// Maximum Codex weekly quota utilization, such as 50%
      #[pound(long)]
      weekly_limit: Option<String>,
      #[pound(long, default = "3600")]
      window_seconds: i64,
      #[pound(long, default = "0")]
      slowdown_ms: i64,
      #[pound(long)]
      prefer_trusted: bool,
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
      /// Limit the report to this account, by id, email or label
      #[pound(long)]
      account: Option<String>,
   },
   /// Show provider-reported quota usage for all accounts as a table
   Accounts,
   /// Replace both user budgets for an account; omitted budgets are unlimited
   Budget {
      #[pound(long)]
      user: String,
      /// Account id, email or label. Omit to apply percentage budgets across the user's fleet.
      #[pound(long)]
      account: Option<String>,
      /// User's estimated share of the five-hour allowance, such as 25%
      #[pound(long = "5hr-budget")]
      five_hour_budget: Option<String>,
      /// User's estimated share of the weekly allowance, such as 25%
      #[pound(long)]
      weekly_budget: Option<String>,
      /// User's estimated spend budget in US dollars
      #[pound(long)]
      usd_budget: Option<String>,
   },
}

#[derive(Parse)]
pub enum DebugCommand {
   /// Send a raw request upstream and dump the SSE events
   Ping {
      #[pound(long)]
      model: Option<String>,
      #[pound(long, default = "Say the word: pong")]
      prompt: String,
   },
   /// Force a token refresh for an account
   Refresh { account: String },
   /// Dump the raw models endpoint response from the codex backend
   Models,
}

pub async fn run(args: Cli, cfg: Config) -> Result<()> {
   let db = Db::open(&cfg.db_path)?;

   match args.command {
      Command::Login { label, provider } => match provider {
         Provider::OpenAi => oauth::login(&db, label).await,
         Provider::Anthropic => anthropic::login(&db, label).await,
         Provider::Gemini => Err(eyre::eyre!(
            "google has no device-code flow here, use `accounts add-key --provider gemini`"
         )),
         Provider::Glm => Err(eyre::eyre!(
            "z.ai issues static keys, use `accounts add-key --provider glm`"
         )),
         Provider::Experiential => Err(eyre::eyre!(
            "experiential issues static keys, use `accounts add-key --provider experiential`"
         )),
         Provider::Zen => Err(eyre::eyre!(
            "zen serves its free models without a credential, use `accounts add-key --provider zen` if you have one"
         )),
      },
      Command::Accounts { command } => match command {
         AccountsCommand::List => accounts_list(&db).await,
         AccountsCommand::AddKey {
            provider,
            key,
            label,
            referer,
         } => accounts_add_key(&db, provider, &key, label.as_deref(), referer.as_deref()).await,
         AccountsCommand::Remove { account } => accounts_remove(&db, &account).await,
         AccountsCommand::Trust { account, off } => accounts_trust(&db, &account, !off).await,
         AccountsCommand::Users { account, allow } => {
            accounts_users(&db, &account, allow.as_deref().unwrap_or_default()).await
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
      Command::Debug { command } => match command {
         DebugCommand::Ping { model, prompt } => codex::debug_ping(&db, &cfg, model, prompt).await,
         DebugCommand::Refresh { account } => debug_refresh(&db, &account).await,
         DebugCommand::Models => codex::debug_models(&db, &cfg).await,
      },
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
) -> Result<()> {
   if referer.is_some() && provider != Provider::Gemini {
      bail!("--referer is only supported for gemini keys");
   }
   let mut hasher = hmac_sha256::Hash::new();
   hasher.update(key.as_bytes());
   let account_id = data_encoding::HEXLOWER.encode(&hasher.finalize()[..8]);
   let tokens = oauth::TokenSet {
      access_token: key.to_owned(),
      refresh_token: String::new(),
      id_token: None,
      expires_at: None,
   };
   let id = db
      .upsert_account(NewAccount {
         provider,
         id: &account_id,
         email: None,
         label,
         plan: None,
         tokens: &tokens,
         auth_mode: AuthMode::ApiKey,
      })
      .await?;
   if let Some(referer) = referer {
      let referer = (!referer.is_empty()).then_some(referer);
      db.set_account_http_referer(id, referer).await?;
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

async fn accounts_trust(db: &Db, account: &str, trusted: bool) -> Result<()> {
   if db.set_account_trusted(account, trusted).await? == 0 {
      bail!("no account matched {account:?}");
   }
   println!(
      "account {account} is now {}",
      if trusted { "trusted" } else { "untrusted" }
   );
   Ok(())
}

async fn accounts_users(db: &Db, account: &str, allow: &str) -> Result<()> {
   let users: Vec<&str> = allow
      .split(',')
      .map(str::trim)
      .filter(|user| !user.is_empty())
      .collect();
   if db
      .set_account_allowed_users(account, &users.join(","))
      .await?
      == 0
   {
      bail!("no account matched {account:?}");
   }
   if users.is_empty() {
      println!("account {account} is now open to every user");
   } else {
      println!("account {account} now serves only {}", users.join(", "));
   }
   Ok(())
}

async fn accounts_remove(db: &Db, account: &str) -> Result<()> {
   let count = db.remove_account(account).await?;
   if count == 0 {
      bail!("no account matched {account:?}");
   }
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
   let Some(found) = db.find_account(&key).await? else {
      bail!("no account matched {key:?}");
   };
   Ok(Some(found.id))
}

fn token_limits(
   requests: Option<i64>,
   tokens: Option<i64>,
   five_hour_limit: Option<String>,
   weekly_limit: Option<String>,
   window_seconds: i64,
   slowdown_ms: i64,
   prefer_trusted: bool,
   providers: Option<String>,
   pinned_account: Option<i64>,
) -> Result<TokenLimits> {
   if requests.is_some_and(|value| value <= 0) {
      bail!("--requests must be greater than zero");
   }
   if tokens.is_some_and(|value| value <= 0) {
      bail!("--tokens must be greater than zero");
   }
   if window_seconds <= 0 {
      bail!("--window-seconds must be greater than zero");
   }
   if slowdown_ms < 0 {
      bail!("--slowdown-ms cannot be negative");
   }
   let five_hour_limit = parse_percentage(five_hour_limit, "--5hr-limit")?;
   let weekly_limit = parse_percentage(weekly_limit, "--weekly-limit")?;
   let providers = providers
      .filter(|csv| !csv.trim().is_empty())
      .map(|raw| {
         raw.split(',')
            .map(|part| {
               Provider::from_str(part).ok_or_else(|| eyre::eyre!("unknown provider: {part}"))
            })
            .collect::<Result<Vec<_>>>()
      })
      .transpose()?
      .unwrap_or_default();
   Ok(TokenLimits {
      requests,
      tokens,
      window_seconds,
      slowdown_ms,
      five_hour_limit,
      weekly_limit,
      prefer_trusted,
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

async fn debug_refresh(db: &Db, account: &str) -> Result<()> {
   let acc = db
      .find_account(account)
      .await?
      .ok_or_else(|| eyre!("no account matched"))?;
   let tokens = match acc.provider {
      Provider::OpenAi => refresh::refresh(&acc.refresh_token).await?,
      Provider::Anthropic => anthropic::refresh(&acc.refresh_token).await?,
      Provider::Gemini | Provider::Zen | Provider::Glm | Provider::Experiential => {
         bail!("this provider has no refresh flow")
      },
   };
   db.update_account_tokens(acc.id, &tokens).await?;
   println!(
      "refreshed account {} ({})",
      acc.id,
      acc.email.as_deref().unwrap_or("-")
   );
   Ok(())
}

#[cfg(test)]
mod tests {
   use super::{
      Cli, Command, QuotaCommand, TokenCommand, parse_percentage, parse_usd_budget, quota_budgets,
   };
   use super::{Db, NewAccount, TokenLimits, quota_command};
   use std::env;

   use crate::config::Config;
   use crate::oauth::TokenSet;
   use crate::provider::{AuthMode, Provider};
   use pound::Parse as _;

   async fn quota_test_database() -> (Db, i64) {
      let path = env::temp_dir().join(format!("slop-cli-{}.db", uuid::Uuid::new_v4()));
      let db = Db::open(&path).unwrap();
      let account = db
         .upsert_account(NewAccount {
            provider: Provider::OpenAi,
            id: "quota-cli-account",
            email: Some("quota@example.com"),
            label: Some("personal"),
            plan: None,
            tokens: &TokenSet {
               access_token: "unused".into(),
               refresh_token: "unused".into(),
               id_token: None,
               expires_at: None,
            },
            auth_mode: AuthMode::OAuth,
         })
         .await
         .unwrap();
      (db, account)
   }

   async fn dispatch_quota(db: &Db, args: &[&str]) -> eyre::Result<String> {
      let cli = Cli::try_parse_from(args.iter().copied()).unwrap();
      let cfg = Config::load(&cli).unwrap();
      let Command::Quota { command } = cli.command else {
         panic!("expected quota command");
      };
      quota_command(db, &cfg, command).await
   }

   #[tokio::test]
   async fn quota_dispatch_replaces_budgets_without_changing_token_limits() {
      let (db, account) = quota_test_database().await;
      let token = db.create_token("kader", "cli-secret", "cli").await.unwrap();
      db.set_token_limits(
         &token.to_string(),
         &TokenLimits {
            requests: Some(60),
            tokens: Some(100_000),
            window_seconds: 3600,
            slowdown_ms: 250,
            five_hour_limit: Some(0.5_f64),
            weekly_limit: Some(0.75_f64),
            prefer_trusted: true,
            pinned_account: Some(account),
            providers: vec![Provider::OpenAi],
         },
      )
      .await
      .unwrap();
      dispatch_quota(
         &db,
         &[
            "quota",
            "budget",
            "--user",
            "kader",
            "--account",
            "personal",
            "--5hr-budget",
            "25%",
            "--weekly-budget",
            "12.5%",
            "--usd-budget",
            "400",
         ],
      )
      .await
      .unwrap();
      let rows = db.user_quota("kader", Some(account)).await.unwrap();
      assert_eq!(rows.len(), 2);
      assert_eq!(
         (rows[0].window_seconds, rows[0].budget_percent),
         (18000, Some(25.0_f64))
      );
      assert_eq!(
         (rows[1].window_seconds, rows[1].budget_percent),
         (604_800, Some(12.5_f64))
      );
      assert!(rows.iter().all(|row| row.spend_budget_usd == Some(400.0)));

      // An invalid second value must not replace the valid first budget.
      let _ = dispatch_quota(
         &db,
         &[
            "quota",
            "budget",
            "--user",
            "kader",
            "--account",
            "personal",
            "--5hr-budget",
            "80%",
            "--weekly-budget",
            "101%",
         ],
      )
      .await
      .unwrap_err();
      assert_eq!(
         db.user_quota("kader", Some(account)).await.unwrap()[0].budget_percent,
         Some(25.0_f64)
      );

      dispatch_quota(
         &db,
         &[
            "quota",
            "budget",
            "--user",
            "kader",
            "--account",
            "quota@example.com",
            "--5hr-budget",
            "30%",
         ],
      )
      .await
      .unwrap();
      let replaced_rows = db.user_quota("kader", Some(account)).await.unwrap();
      assert_eq!(replaced_rows.len(), 1);
      assert_eq!(replaced_rows[0].budget_percent, Some(30.0_f64));
      dispatch_quota(
         &db,
         &[
            "quota",
            "budget",
            "--user",
            "kader",
            "--account",
            &account.to_string(),
         ],
      )
      .await
      .unwrap();
      assert!(
         db.user_quota("kader", Some(account))
            .await
            .unwrap()
            .is_empty()
      );

      let limits = db.auth_token("cli-secret").await.unwrap().unwrap().limits;
      assert_eq!(limits.requests, Some(60));
      assert_eq!(limits.tokens, Some(100_000));
      assert_eq!(limits.window_seconds, 3600);
      assert_eq!(limits.slowdown_ms, 250);
      assert_eq!(limits.five_hour_limit, Some(0.5_f64));
      assert_eq!(limits.weekly_limit, Some(0.75_f64));
      assert!(limits.prefer_trusted);
      assert_eq!(limits.pinned_account, Some(account));
      assert_eq!(limits.providers, vec![Provider::OpenAi]);
   }

   #[tokio::test]
   async fn quota_dispatch_without_account_sets_fleet_percentage_budgets() {
      let (db, account) = quota_test_database().await;
      dispatch_quota(
         &db,
         &[
            "quota",
            "budget",
            "--user",
            "kader",
            "--5hr-budget",
            "50%",
            "--weekly-budget",
            "25%",
         ],
      )
      .await
      .unwrap();
      let rows = db.user_quota("kader", None).await.unwrap();
      let fleet = rows
         .iter()
         .find(|row| row.account_id.is_none())
         .expect("fleet row");
      assert_eq!(fleet.window_seconds, 18_000);
      assert_eq!(fleet.budget_percent, Some(50.0));
      assert_eq!(fleet.fleet_capacity_points, Some(0.0));
      assert!(rows.iter().all(|row| row.account_id != Some(account)));

      // Cross-flag validation happens before replacing either fleet window.
      let error = dispatch_quota(
         &db,
         &[
            "quota",
            "budget",
            "--user",
            "kader",
            "--5hr-budget",
            "80%",
            "--usd-budget",
            "400",
         ],
      )
      .await
      .unwrap_err()
      .to_string();
      assert!(error.contains("--usd-budget requires --account"), "{error}");
      assert_eq!(
         db.user_quota("kader", None)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.account_id.is_none() && row.window_seconds == 18_000)
            .and_then(|row| row.budget_percent),
         Some(50.0)
      );
   }

   #[tokio::test]
   async fn quota_usage_dispatch_serializes_unknown_observations_as_null() {
      let (db, account) = quota_test_database().await;
      let empty = dispatch_quota(&db, &["quota", "usage", "--user", "kader"])
         .await
         .unwrap();
      assert_eq!(
         serde_json::from_str::<serde_json::Value>(&empty).unwrap(),
         serde_json::json!([])
      );
      db.set_user_quota_budget("kader", account, 18000, Some(25.0_f64))
         .await
         .unwrap();
      let json = dispatch_quota(
         &db,
         &["quota", "usage", "--user", "kader", "--account", "personal"],
      )
      .await
      .unwrap();
      let rows: serde_json::Value = serde_json::from_str(&json).unwrap();
      assert_eq!(rows[0]["account_id"], account);
      assert_eq!(rows[0]["user"], "kader");
      assert_eq!(rows[0]["budget_percent"], 25.0_f64);
      assert_eq!(rows[0]["estimated"], true);
      assert!(rows[0]["observed_used_percent"].is_null());
      assert!(rows[0]["resets_at"].is_null());
      assert!(rows[0]["allowance_used_percent"].is_null());
      let _ = dispatch_quota(
         &db,
         &["quota", "usage", "--user", "kader", "--account", "missing"],
      )
      .await
      .unwrap_err();
   }

   #[test]
   fn quota_usage_accepts_optional_account() {
      for account in [None, Some("1"), Some("kader@example.com"), Some("personal")] {
         let mut args = vec!["quota", "usage", "--user", "kader"];
         if let Some(account) = account {
            args.extend(["--account", account]);
         }
         let cli = Cli::try_parse_from(args).unwrap();
         let Command::Quota {
            command:
               QuotaCommand::Usage {
                  user,
                  account: parsed,
               },
         } = cli.command
         else {
            panic!("expected quota usage");
         };
         assert_eq!(user, "kader");
         assert_eq!(parsed.as_deref(), account);
      }
   }

   #[test]
   fn quota_budget_accepts_percentage_flags() {
      let cli = Cli::try_parse_from([
         "quota",
         "budget",
         "--user",
         "kader",
         "--account",
         "personal",
         "--5hr-budget",
         "25%",
         "--weekly-budget",
         "12.5%",
      ])
      .unwrap();
      let Command::Quota {
         command:
            QuotaCommand::Budget {
               user,
               account,
               five_hour_budget,
               weekly_budget,
               usd_budget,
            },
      } = cli.command
      else {
         panic!("expected quota budget");
      };
      assert_eq!(user, "kader");
      assert_eq!(account.as_deref(), Some("personal"));
      assert_eq!(usd_budget, None);
      assert_eq!(
         quota_budgets(five_hour_budget, weekly_budget).unwrap(),
         (Some(25.0_f64), Some(12.5_f64))
      );
   }

   #[test]
   fn quota_budget_omitted_windows_are_unlimited() {
      let cli =
         Cli::try_parse_from(["quota", "budget", "--user", "kader", "--account", "1"]).unwrap();
      let Command::Quota {
         command:
            QuotaCommand::Budget {
               five_hour_budget,
               weekly_budget,
               usd_budget,
               ..
            },
      } = cli.command
      else {
         panic!("expected quota budget");
      };
      assert_eq!(usd_budget, None);
      assert_eq!(
         quota_budgets(five_hour_budget, weekly_budget).unwrap(),
         (None, None)
      );
      assert_eq!(
         quota_budgets(Some("25%".into()), None).unwrap(),
         (Some(25.0_f64), None)
      );
      assert_eq!(
         quota_budgets(None, Some("25%".into())).unwrap(),
         (None, Some(25.0_f64))
      );
   }

   #[test]
   fn quota_budget_accepts_finite_nonnegative_usd_amounts() {
      for (raw, expected) in [("0", 0.0), ("400", 400.0), ("12.50", 12.5), ("$400", 400.0)] {
         assert_eq!(
            parse_usd_budget(Some(raw.into()), "--usd-budget").unwrap(),
            Some(expected)
         );
      }
      assert_eq!(parse_usd_budget(None, "--usd-budget").unwrap(), None);
      for raw in ["-1", "NaN", "inf", "$", "$-1", "garbage"] {
         assert!(parse_usd_budget(Some(raw.into()), "--usd-budget").is_err());
      }
   }

   #[test]
   fn quota_budget_requires_usd_amount_when_flag_is_present() {
      let cli = Cli::try_parse_from([
         "quota",
         "budget",
         "--user",
         "kader",
         "--account",
         "personal",
         "--usd-budget",
         "$400",
      ])
      .unwrap();
      let Command::Quota {
         command: QuotaCommand::Budget { usd_budget, .. },
      } = cli.command
      else {
         panic!("expected quota budget");
      };
      assert_eq!(
         parse_usd_budget(usd_budget, "--usd-budget").unwrap(),
         Some(400.0)
      );
   }

   #[test]
   fn quota_commands_require_user_but_budget_account_is_optional() {
      assert!(Cli::try_parse_from(["quota", "usage"]).is_err());
      assert!(Cli::try_parse_from(["quota", "budget", "--account", "1"]).is_err());
      let cli = Cli::try_parse_from(["quota", "budget", "--user", "kader"]).unwrap();
      let Command::Quota {
         command: QuotaCommand::Budget { account, .. },
      } = cli.command
      else {
         panic!("expected quota budget");
      };
      assert_eq!(account, None);
   }

   #[test]
   fn quota_budget_rejects_usd_without_account_before_dispatch() {
      let cli = Cli::try_parse_from(["quota", "budget", "--user", "kader", "--usd-budget", "400"])
         .unwrap();
      let Command::Quota { command } = cli.command else {
         panic!("expected quota");
      };
      // Dispatch performs this cross-flag validation because the parser accepts
      // each flag independently.
      assert!(matches!(
         command,
         QuotaCommand::Budget {
            account: None,
            usd_budget: Some(_),
            ..
         }
      ));
   }

   #[test]
   fn quota_budgets_accept_only_bounded_finite_percentages() {
      assert_eq!(
         quota_budgets(Some("0%".into()), Some("100%".into())).unwrap(),
         (Some(0.0_f64), Some(100.0_f64))
      );
      for value in ["25", "101%", "-1%", "NaN%", "inf%", "garbage%"] {
         let _ = quota_budgets(Some(value.into()), None).unwrap_err();
         let _ = quota_budgets(None, Some(value.into())).unwrap_err();
      }
   }

   #[test]
   fn token_create_accepts_codex_percentage_flags() {
      let cli = Cli::try_parse_from([
         "token",
         "create",
         "--user",
         "alice",
         "--5hr-limit",
         "50%",
         "--weekly-limit",
         "50%",
      ])
      .unwrap();
      let Command::Token {
         command:
            TokenCommand::Create {
               five_hour_limit,
               weekly_limit,
               ..
            },
      } = cli.command
      else {
         panic!("expected token create");
      };
      assert_eq!(five_hour_limit.as_deref(), Some("50%"));
      assert_eq!(weekly_limit.as_deref(), Some("50%"));
   }

   #[test]
   fn codex_percentages_are_stored_as_fractions() {
      assert_eq!(
         parse_percentage(Some("50%".into()), "--5hr-limit").unwrap(),
         Some(0.5)
      );
      assert_eq!(parse_percentage(None, "--5hr-limit").unwrap(), None);
   }

   #[test]
   fn codex_percentages_must_be_bounded_and_marked() {
      assert!(parse_percentage(Some("50".into()), "--weekly-limit").is_err());
      assert!(parse_percentage(Some("101%".into()), "--weekly-limit").is_err());
      assert!(parse_percentage(Some("-1%".into()), "--weekly-limit").is_err());
   }
}
