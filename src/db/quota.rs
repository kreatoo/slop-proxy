//! Estimated subscription usage, measured in percentage points, not token limits.
//!
//! Only provider polling feeds this ledger. Provider samples can be delayed or
//! rounded, and requests settle asynchronously. Deltas are estimates apportioned
//! among newly settled local work, not exact bills. External traffic cannot be
//! distinguished from local traffic in a shared provider reading. A baseline and
//! deltas with no local work remain unattributed; historical work is never guessed.

use eyre::{Result, ensure};
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use serde::Serialize;

use super::Db;
use crate::clock;

#[derive(Debug, Clone)]
pub struct QuotaObservation {
   pub account_id: i64,
   pub window_seconds: i64,
   pub resets_at: i64,
   pub used_percent: f64,
   pub observed_at: i64,
}

#[derive(Debug, Serialize)]
pub struct UserQuotaReport {
   pub user: String,
   pub account_id: i64,
   pub provider: String,
   pub window_seconds: i64,
   /// None means no current provider reading, rather than zero provider usage.
   pub resets_at: Option<i64>,
   pub observed_used_percent: Option<f64>,
   /// Percentage points of the account's full subscription window.
   /// Zero without an observation means no known attribution, not measured zero.
   pub estimated_user_percent: f64,
   pub budget_percent: Option<f64>,
   /// Lifetime settled list-price spend for this user and account. Spend
   /// budgets have no automatic reset.
   pub estimated_cost_usd: f64,
   pub spend_budget_usd: Option<f64>,
   /// Lifetime spend / assigned spend budget * 100. Undefined for zero.
   pub spend_allowance_used_percent: Option<f64>,
   /// Estimated usage / assigned budget * 100. Undefined for a zero budget.
   pub allowance_used_percent: Option<f64>,
   pub baseline_percent: Option<f64>,
   /// Includes the initial baseline and later deltas without local work.
   pub unattributed_percent: Option<f64>,
   pub observed_at: Option<i64>,
   pub estimated: bool,
}

fn validate_window(window_seconds: i64) -> Result<()> {
   ensure!(
      matches!(window_seconds, 18000 | 604_800),
      "quota window must be 18000 or 604800 seconds"
   );
   Ok(())
}

fn validate_account(conn: &Connection, account_id: i64) -> Result<()> {
   let provider: Option<String> = conn
      .query_row(
         "SELECT provider FROM accounts WHERE id = ?1",
         [account_id],
         |row| row.get(0),
      )
      .optional()?;
   ensure!(
      provider.as_deref() == Some("openai"),
      "quota accounting requires an existing openai account"
   );
   Ok(())
}

impl Db {
   pub async fn observe_quota(&self, observation: QuotaObservation) -> Result<()> {
      validate_window(observation.window_seconds)?;
      ensure!(
         observation.used_percent.is_finite()
            && (0.0_f64..=100.0_f64).contains(&observation.used_percent),
         "quota usage must be finite and between 0 and 100 percent"
      );
      ensure!(
         observation.observed_at >= 0 && observation.resets_at > observation.observed_at,
         "quota reading requires a future reset and nonnegative observation time"
      );
      self.call(move |conn| observe(conn, &observation)).await
   }

   pub async fn user_quota(
      &self,
      user: &str,
      account_id: Option<i64>,
   ) -> Result<Vec<UserQuotaReport>> {
      ensure!(!user.trim().is_empty(), "quota user must not be empty");
      let user = user.to_owned();
      self
         .reports
         .call(move |conn| reports(conn, &user, account_id, clock::unix_now()))
         .await
   }

   /// Set or clear a lifetime spend budget. There is no automatic reset;
   /// callers can clear and replace the budget explicitly.
   pub async fn set_user_spend_budget(
      &self,
      user: &str,
      account_id: i64,
      budget_usd: Option<f64>,
   ) -> Result<()> {
      ensure!(!user.trim().is_empty(), "quota user must not be empty");
      if let Some(budget) = budget_usd {
         ensure!(
            budget.is_finite() && budget >= 0.0_f64,
            "spend budget must be finite and nonnegative"
         );
      }
      let user = user.to_owned();
      self.call(move |conn| {
         validate_account(conn, account_id)?;
         if let Some(budget) = budget_usd {
            conn.execute(
               "INSERT INTO user_spend_budgets (user, account_id, budget_usd)
                VALUES (?1, ?2, ?3) ON CONFLICT(user, account_id)
                DO UPDATE SET budget_usd = excluded.budget_usd",
               params![user, account_id, budget],
            )?;
         } else {
            conn.execute(
               "DELETE FROM user_spend_budgets WHERE user = ?1 AND account_id = ?2",
               params![user, account_id],
            )?;
         }
         Ok(())
      }).await
   }

   /// Return a fixed retry delay when a lifetime spend budget is exhausted.
   /// Spend budgets have no provider reset to use for a more precise delay.
   pub async fn user_spend_retry_after(
      &self,
      user: &str,
      account_id: i64,
   ) -> Result<Option<i64>> {
      ensure!(!user.trim().is_empty(), "quota user must not be empty");
      let user = user.to_owned();
      self.call(move |conn| spend_retry_after(conn, &user, account_id)).await
   }

   // Test fixtures can update one window; the CLI replaces both atomically.
   #[cfg(test)]
   pub async fn set_user_quota_budget(
      &self,
      user: &str,
      account_id: i64,
      window_seconds: i64,
      percent: Option<f64>,
   ) -> Result<()> {
      ensure!(!user.trim().is_empty(), "quota user must not be empty");
      validate_window(window_seconds)?;
      if let Some(percent) = percent {
         ensure!(
            percent.is_finite() && (0.0_f64..=100.0_f64).contains(&percent),
            "quota budget must be finite and between 0 and 100 percent"
         );
      }
      let user = user.to_owned();
      self.call(move |conn| {
         validate_account(conn, account_id)?;
         if let Some(percent) = percent {
            conn.execute("INSERT INTO user_quota_budgets (user, account_id, window_seconds, budget_percent)
               VALUES (?1, ?2, ?3, ?4) ON CONFLICT(user, account_id, window_seconds)
               DO UPDATE SET budget_percent = excluded.budget_percent", params![user, account_id, window_seconds, percent])?;
         } else {
            conn.execute("DELETE FROM user_quota_budgets WHERE user = ?1 AND account_id = ?2 AND window_seconds = ?3", params![user, account_id, window_seconds])?;
         }
         Ok(())
      }).await
   }

   /// Replace both budgets together. None clears that window's budget.
   pub async fn set_user_quota_budgets(
      &self,
      user: &str,
      account_id: i64,
      five_hour: Option<f64>,
      weekly: Option<f64>,
   ) -> Result<()> {
      ensure!(!user.trim().is_empty(), "quota user must not be empty");
      for percent in [five_hour, weekly].into_iter().flatten() {
         ensure!(
            percent.is_finite() && (0.0_f64..=100.0_f64).contains(&percent),
            "quota budget must be finite and between 0 and 100 percent"
         );
      }
      let user = user.to_owned();
      self.call(move |conn| {
         let txn = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
         validate_account(&txn, account_id)?;
         for (window, percent) in [(18000, five_hour), (604_800, weekly)] {
            if let Some(percent) = percent {
               txn.execute("INSERT INTO user_quota_budgets (user, account_id, window_seconds, budget_percent)
                  VALUES (?1, ?2, ?3, ?4) ON CONFLICT(user, account_id, window_seconds)
                  DO UPDATE SET budget_percent = excluded.budget_percent", params![user, account_id, window, percent])?;
            } else {
               txn.execute("DELETE FROM user_quota_budgets WHERE user = ?1 AND account_id = ?2 AND window_seconds = ?3", params![user, account_id, window])?;
            }
         }
         txn.commit()?;
         Ok(())
      }).await
   }

   pub async fn user_quota_retry_after(&self, user: &str, account_id: i64) -> Result<Option<i64>> {
      ensure!(!user.trim().is_empty(), "quota user must not be empty");
      let user = user.to_owned();
      // Admission stays on the writer worker, never behind expensive reports.
      self
         .call(move |conn| retry_after(conn, &user, account_id, clock::unix_now()))
         .await
   }
}

fn spend_cost(conn: &Connection, user: &str, account_id: i64) -> Result<f64> {
   conn
      .query_row(
         "SELECT COALESCE(SUM(list_cost_usd), 0.0) FROM usage_log
          WHERE user = ?1 AND account_id = ?2 AND provider = 'openai'
            AND status > 0
            AND CASE WHEN upstream_model = '' THEN requested_model ELSE upstream_model END
                <> 'gpt-5.3-codex-spark'",
         params![user, account_id],
         |row| row.get(0),
      )
      .map_err(Into::into)
}

fn spend_retry_after(conn: &Connection, user: &str, account_id: i64) -> Result<Option<i64>> {
   let Some(budget) = conn
      .query_row(
         "SELECT budget_usd FROM user_spend_budgets WHERE user = ?1 AND account_id = ?2",
         params![user, account_id],
         |row| row.get::<_, f64>(0),
      )
      .optional()?
   else {
      return Ok(None);
   };
   Ok((spend_cost(conn, user, account_id)? >= budget).then_some(60))
}

fn retry_after(conn: &Connection, user: &str, account_id: i64, now: i64) -> Result<Option<i64>> {
   Ok(reports(conn, user, Some(account_id), now)?
      .into_iter()
      .filter_map(|report| {
         report
            .budget_percent
            .filter(|budget| report.estimated_user_percent >= *budget)
            .map(|_| {
               report
                  .resets_at
                  .map_or(60, |reset| reset.saturating_sub(now).max(1))
            })
      })
      .max())
}

fn observe(conn: &mut Connection, sample: &QuotaObservation) -> Result<()> {
   let txn = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
   validate_account(&txn, sample.account_id)?;
   txn.execute("INSERT INTO quota_observations (account_id, window_seconds, resets_at, used_percent, observed_at)
      VALUES (?1, ?2, ?3, ?4, ?5)", params![sample.account_id, sample.window_seconds, sample.resets_at, sample.used_percent, sample.observed_at])?;
   let previous: Option<(i64, i64, f64, i64)> = txn
      .query_row(
         "SELECT resets_at, observed_at, high_water_percent, usage_cursor FROM quota_epochs
       WHERE account_id = ?1 AND window_seconds = ?2 ORDER BY resets_at DESC LIMIT 1",
         params![sample.account_id, sample.window_seconds],
         |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
      )
      .optional()?;
   if previous.is_some_and(|(reset, observed, _, _)| {
      sample.resets_at < reset || sample.observed_at <= observed
   }) {
      // Preserve stale samples for auditing, but never move a ledger backwards.
      txn.commit()?;
      return Ok(());
   }
   let end_cursor: i64 = txn.query_row(
      "SELECT COALESCE(MAX(id), 0) FROM usage_log WHERE account_id = ?1 AND provider = 'openai' AND ts <= ?2",
      params![sample.account_id, sample.observed_at], |row| row.get(0),
   )?;
   match previous {
      Some((reset, _, high_water, cursor)) if reset == sample.resets_at => {
         let delta = (sample.used_percent - high_water).max(0.0);
         let mut unattributed = 0.0_f64;
         if delta > 0.0_f64 {
            let work = {
               let mut stmt = txn.prepare(
                  "SELECT user, list_cost_usd,
                  MAX(input_tokens, 0) + MAX(output_tokens, 0) * 4.0
                  + MAX(cache_read_tokens, 0) * 0.1 + MAX(cache_write_tokens, 0) * 1.25
                  FROM usage_log WHERE account_id = ?1 AND provider = 'openai'
                  AND id > ?2 AND id <= ?3 AND ts <= ?4
                  AND lower(CASE WHEN upstream_model = '' THEN requested_model ELSE upstream_model END) != 'gpt-5.3-codex-spark'",
               )?;
               stmt
                  .query_map(
                     params![sample.account_id, cursor, end_cursor, sample.observed_at],
                     |row| {
                        Ok((
                           row.get::<_, String>(0)?,
                           row.get::<_, f64>(1)?,
                           row.get::<_, f64>(2)?,
                        ))
                     },
                  )?
                  .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let work: Vec<_> = work
               .into_iter()
               .filter(|&(_, _, weight)| weight.is_finite() && weight > 0.0_f64)
               .collect();
            // Never mix dollars and tokens within one interval. One unpriced
            // request switches every request in the interval to token weights.
            let priced = work
               .iter()
               .all(|&(_, cost, _)| cost.is_finite() && cost > 0.0_f64);
            let total: f64 = work
               .iter()
               .map(|&(_, cost, fallback)| if priced { cost } else { fallback })
               .sum();
            if total > 0.0_f64 && total.is_finite() {
               for (user, cost, fallback) in work {
                  let share = delta * (if priced { cost } else { fallback } / total);
                  txn.execute("INSERT INTO user_quota_estimates (user, account_id, window_seconds, resets_at, used_percent)
                     VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(user, account_id, window_seconds, resets_at)
                     DO UPDATE SET used_percent = used_percent + excluded.used_percent",
                     params![user, sample.account_id, sample.window_seconds, sample.resets_at, share])?;
               }
            } else {
               unattributed = delta;
            }
         }
         // Rounded unchanged readings keep the cursor: small requests remain
         // eligible when the next positive delta arrives. Decreases keep the
         // high-water mark too, so a correction/rebound cannot charge twice.
         txn.execute("UPDATE quota_epochs SET observed_percent = ?4, observed_at = ?5,
            high_water_percent = MAX(high_water_percent, ?4), unattributed_percent = unattributed_percent + ?6,
            usage_cursor = ?7 WHERE account_id = ?1 AND window_seconds = ?2 AND resets_at = ?3",
            params![sample.account_id, sample.window_seconds, sample.resets_at, sample.used_percent,
               sample.observed_at, unattributed, if delta > 0.0_f64 { end_cursor.max(cursor) } else { cursor }])?;
      },
      _ => {
         // Each reset starts from what we actually observed, not from guessed
         // historical attribution. Work preceding this baseline is excluded.
         txn.execute("INSERT INTO quota_epochs (account_id, window_seconds, resets_at,
            observed_percent, high_water_percent, baseline_percent, unattributed_percent, observed_at, usage_cursor)
            VALUES (?1, ?2, ?3, ?4, ?4, ?4, ?4, ?5, ?6)",
            params![sample.account_id, sample.window_seconds, sample.resets_at, sample.used_percent, sample.observed_at, end_cursor])?;
      },
   }
   txn.commit()?;
   Ok(())
}

fn reports(
   conn: &Connection,
   user: &str,
   account_id: Option<i64>,
   now: i64,
) -> Result<Vec<UserQuotaReport>> {
   let mut stmt = conn.prepare("WITH current AS (
      SELECT e.* FROM quota_epochs e WHERE e.resets_at > ?3 AND (?2 IS NULL OR e.account_id = ?2) AND NOT EXISTS (
         SELECT 1 FROM quota_epochs newer WHERE newer.account_id = e.account_id
         AND newer.window_seconds = e.window_seconds AND newer.resets_at > e.resets_at)
   ), base_windows AS (
      SELECT account_id, window_seconds FROM current
      UNION SELECT account_id, window_seconds FROM user_quota_budgets WHERE user = ?1 AND (?2 IS NULL OR account_id = ?2)
   ), windows AS (
      SELECT account_id, window_seconds FROM base_windows
      UNION SELECT sb.account_id, 0 FROM user_spend_budgets sb
         WHERE sb.user = ?1 AND (?2 IS NULL OR sb.account_id = ?2)
           AND NOT EXISTS (SELECT 1 FROM base_windows bw WHERE bw.account_id = sb.account_id)
   ), costs AS (
      SELECT user, account_id, COALESCE(SUM(list_cost_usd), 0.0) AS estimated_cost_usd
      FROM usage_log
      WHERE provider = 'openai' AND status > 0
        AND CASE WHEN upstream_model = '' THEN requested_model ELSE upstream_model END
            <> 'gpt-5.3-codex-spark'
      GROUP BY user, account_id
   ) SELECT w.account_id, a.provider, w.window_seconds, e.resets_at,
      e.observed_percent, COALESCE(u.used_percent, 0), b.budget_percent,
      e.baseline_percent, e.unattributed_percent, e.observed_at,
      COALESCE(c.estimated_cost_usd, 0.0), sb.budget_usd
      FROM windows w JOIN accounts a ON a.id = w.account_id
      LEFT JOIN current e ON e.account_id = w.account_id AND e.window_seconds = w.window_seconds
      LEFT JOIN user_quota_estimates u ON u.account_id = e.account_id AND u.window_seconds = e.window_seconds
         AND u.resets_at = e.resets_at AND u.user = ?1
      LEFT JOIN user_quota_budgets b ON b.account_id = w.account_id AND b.window_seconds = w.window_seconds AND b.user = ?1
      LEFT JOIN costs c ON c.account_id = w.account_id AND c.user = ?1
      LEFT JOIN user_spend_budgets sb ON sb.account_id = w.account_id AND sb.user = ?1
      WHERE (?2 IS NULL OR w.account_id = ?2) AND a.provider = 'openai'
      ORDER BY w.account_id, w.window_seconds")?;
   let rows = stmt
      .query_map(params![user, account_id, now], |row| {
         let consumption: f64 = row.get(5)?;
         let budget: Option<f64> = row.get(6)?;
         let resets_at: Option<i64> = row.get(3)?;
         let estimated_cost_usd: f64 = row.get(10)?;
         let spend_budget_usd: Option<f64> = row.get(11)?;
         Ok(UserQuotaReport {
            user: user.to_owned(),
            account_id: row.get(0)?,
            provider: row.get(1)?,
            window_seconds: row.get(2)?,
            resets_at,
            observed_used_percent: row.get(4)?,
            estimated_user_percent: consumption,
            budget_percent: budget,
            allowance_used_percent: budget
               .filter(|value| *value > 0.0_f64 && resets_at.is_some())
               .map(|value| consumption / value * 100.0_f64),
            estimated_cost_usd,
            spend_budget_usd,
            spend_allowance_used_percent: spend_budget_usd
               .filter(|value| *value > 0.0_f64)
               .map(|value| estimated_cost_usd / value * 100.0_f64),
            baseline_percent: row.get(7)?,
            unattributed_percent: row.get(8)?,
            observed_at: row.get(9)?,
            estimated: true,
         })
      })?
      .collect::<rusqlite::Result<Vec<_>>>()?;
   Ok(rows)
}

#[cfg(test)]
mod tests {
   use super::*;
   use crate::db::usage::UsageRecord;
   use crate::provider::Provider;
   use std::path::PathBuf;

   fn database() -> (Db, PathBuf) {
      let path = std::env::temp_dir().join(format!("slop-quota-{}.db", uuid::Uuid::new_v4()));
      (Db::open(&path).unwrap(), path)
   }

   async fn account(db: &Db, provider: &str) -> i64 {
      let provider = provider.to_owned();
      db.call(move |conn| {
         conn.execute(
            "INSERT INTO accounts (provider, provider_account_id, access_token, refresh_token)
            VALUES (?1, ?2, '', '')",
            params![provider, uuid::Uuid::new_v4().to_string()],
         )?;
         Ok(conn.last_insert_rowid())
      })
      .await
      .unwrap()
   }

   async fn sample(db: &Db, id: i64, reset: i64, at: i64, percent: f64) {
      db.observe_quota(QuotaObservation {
         account_id: id,
         window_seconds: 18000,
         resets_at: reset,
         used_percent: percent,
         observed_at: at,
      })
      .await
      .unwrap();
   }

   async fn work(db: &Db, id: i64, user: &str, cost: f64, input: i64, output: i64) {
      db.enqueue_usage(UsageRecord {
         account_id: Some(id),
         user: user.into(),
         provider: Some(Provider::OpenAi),
         list_cost_usd: cost,
         input_tokens: input,
         output_tokens: output,
         status: 200,
         ..Default::default()
      })
      .unwrap();
      db.flush().await.unwrap();
   }

   async fn report(db: &Db, user: &str, id: i64) -> UserQuotaReport {
      db.user_quota(user, Some(id)).await.unwrap().remove(0)
   }

   fn close(left: f64, right: f64) {
      assert!((left - right).abs() < 1e-9, "{left} != {right}");
   }

   #[tokio::test]
   async fn baseline_then_priced_split_and_rounded_carry_over() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let now = clock::unix_now();
      let reset = now + 18000;
      work(&db, id, "alice", 100.0, 100, 0).await;
      sample(&db, id, reset, now, 44.0).await;
      let baseline = report(&db, "alice", id).await;
      close(baseline.estimated_user_percent, 0.0);
      assert_eq!(baseline.baseline_percent, Some(44.0));
      assert_eq!(baseline.unattributed_percent, Some(44.0));
      assert!(baseline.estimated);
      work(&db, id, "alice", 1.0, 100, 0).await;
      sample(&db, id, reset, now + 1, 44.0).await;
      sample(&db, id, reset, now + 2, 44.0).await;
      work(&db, id, "bob", 3.0, 100, 0).await;
      sample(&db, id, reset, now + 3, 48.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 1.0);
      close(report(&db, "bob", id).await.estimated_user_percent, 3.0);
      sample(&db, id, reset, now + 4, 49.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 1.0);
      assert_eq!(
         report(&db, "alice", id).await.unattributed_percent,
         Some(45.0)
      );
   }

   #[tokio::test]
   async fn reset_stale_decrease_and_rebound_never_double_charge() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let now = clock::unix_now();
      let reset = now + 18000;
      sample(&db, id, reset, now, 20.0).await;
      work(&db, id, "alice", 1.0, 10, 0).await;
      sample(&db, id, reset, now + 2, 25.0).await;
      sample(&db, id, reset, now + 1, 50.0).await;
      sample(&db, id, reset, now + 2, 60.0).await;
      sample(&db, id, reset, now + 3, 10.0).await;
      work(&db, id, "alice", 1.0, 10, 0).await;
      sample(&db, id, reset, now + 4, 25.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 5.0);
      sample(&db, id, reset, now + 5, 27.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 7.0);
      sample(&db, id, reset + 18000, now + 6, 3.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 0.0);
      sample(&db, id, reset, now + 7, 90.0).await;
      assert_eq!(
         report(&db, "alice", id).await.resets_at,
         Some(reset + 18000)
      );
      work(&db, id, "alice", 1.0, 10, 0).await;
      sample(&db, id, reset + 18000, now + 8, 4.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 1.0);
      db.call(move |conn| {
         let epochs: i64 = conn.query_row("SELECT COUNT(*) FROM quota_epochs", [], |r| r.get(0))?;
         let observations: i64 =
            conn.query_row("SELECT COUNT(*) FROM quota_observations", [], |r| r.get(0))?;
         let history: f64 = conn.query_row(
            "SELECT used_percent FROM user_quota_estimates WHERE resets_at = ?1",
            [reset],
            |r| r.get(0),
         )?;
         assert_eq!(epochs, 2);
         assert_eq!(observations, 10);
         close(history, 7.0);
         Ok(())
      })
      .await
      .unwrap();
   }

   #[tokio::test]
   async fn fallback_applies_to_entire_interval_including_cached_work() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let now = clock::unix_now();
      sample(&db, id, now + 18000, now, 0.0).await;
      work(&db, id, "alice", 99.0, 100, 0).await;
      work(&db, id, "bob", 0.0, 0, 50).await;
      db.enqueue_usage(UsageRecord {
         account_id: Some(id),
         user: "carol".into(),
         provider: Some(Provider::OpenAi),
         cache_read_tokens: 500,
         cache_write_tokens: 40,
         status: 200,
         ..Default::default()
      })
      .unwrap();
      sample(&db, id, now + 18000, now + 1, 8.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 2.0);
      close(report(&db, "bob", id).await.estimated_user_percent, 4.0);
      close(report(&db, "carol", id).await.estimated_user_percent, 2.0);
   }

   #[tokio::test]
   async fn excludes_spark_zero_usage_other_provider_and_other_accounts() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let other = account(&db, "openai").await;
      let now = clock::unix_now();
      sample(&db, id, now + 18000, now, 5.0).await;
      sample(&db, other, now + 18000, now, 5.0).await;
      work(&db, other, "alice", 1.0, 100, 0).await;
      work(&db, id, "empty", 10.0, 0, 0).await;
      for (requested, upstream) in [
         ("GPT-5.3-Codex-Spark", ""),
         ("alias", "gpt-5.3-codex-spark"),
      ] {
         db.enqueue_usage(UsageRecord {
            account_id: Some(id),
            user: "spark".into(),
            provider: Some(Provider::OpenAi),
            input_tokens: 100,
            requested_model: requested.into(),
            upstream_model: upstream.into(),
            status: 200,
            ..Default::default()
         })
         .unwrap();
      }
      db.call(move |conn| {
         conn.execute("INSERT INTO usage_log(user,account_id,provider,dialect,requested_model,upstream_model,input_tokens,status)
            VALUES ('foreign',?1,'anthropic','','','',100,200)", [id])?;
         Ok(())
      }).await.unwrap();
      sample(&db, id, now + 18000, now + 1, 9.0).await;
      for user in ["alice", "empty", "spark", "foreign"] {
         close(report(&db, user, id).await.estimated_user_percent, 0.0);
      }
      assert_eq!(
         report(&db, "alice", id).await.unattributed_percent,
         Some(9.0)
      );
      sample(&db, other, now + 18000, now + 1, 7.0).await;
      close(
         report(&db, "alice", other).await.estimated_user_percent,
         2.0,
      );
   }

   #[tokio::test]
   async fn rejects_invalid_readings_and_budget_inputs() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let foreign = account(&db, "anthropic").await;
      let now = clock::unix_now();
      for percent in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.01, 100.01] {
         assert!(
            db.observe_quota(QuotaObservation {
               account_id: id,
               window_seconds: 18000,
               resets_at: now + 18000,
               used_percent: percent,
               observed_at: now
            })
            .await
            .is_err()
         );
         assert!(
            db.set_user_quota_budget("alice", id, 18000, Some(percent))
               .await
               .is_err()
         );
      }
      for (account_id, window_seconds, resets_at, observed_at) in [
         (id, 0, now + 18000, now),
         (id, 18000, now, now),
         (id, 18000, now, -1),
         (foreign, 18000, now + 18000, now),
         (9999, 18000, now + 18000, now),
      ] {
         assert!(
            db.observe_quota(QuotaObservation {
               account_id,
               window_seconds,
               resets_at,
               observed_at,
               used_percent: 0.0
            })
            .await
            .is_err()
         );
      }
      for (user, account_id, window) in [
         (" ", id, 18000),
         ("alice", foreign, 18000),
         ("alice", 9999, 18000),
         ("alice", id, 42),
      ] {
         assert!(
            db.set_user_quota_budget(user, account_id, window, Some(10.0))
               .await
               .is_err()
         );
      }
      db.call(|conn| {
         let count: i64 =
            conn.query_row("SELECT COUNT(*) FROM quota_observations", [], |r| r.get(0))?;
         assert_eq!(count, 0);
         Ok(())
      })
      .await
      .unwrap();
   }

   #[tokio::test]
   async fn budgets_share_tokens_persist_and_expire_with_epochs() {
      let (db, path) = database();
      let id = account(&db, "openai").await;
      let now = clock::unix_now();
      let reset = now + 18000;
      let token1 = db
         .create_token("alice", "first-secret", "one")
         .await
         .unwrap();
      let token2 = db
         .create_token("alice", "second-secret", "two")
         .await
         .unwrap();
      db.set_user_quota_budget("alice", id, 18000, Some(4.0))
         .await
         .unwrap();
      let unknown = report(&db, "alice", id).await;
      assert_eq!(unknown.observed_used_percent, None);
      assert_eq!(unknown.resets_at, None);
      assert_eq!(db.user_quota_retry_after("alice", id).await.unwrap(), None);
      sample(&db, id, reset, now, 44.0).await;
      for token_id in [token1, token2] {
         db.enqueue_usage(UsageRecord {
            token_id: Some(token_id),
            account_id: Some(id),
            user: "alice".into(),
            provider: Some(Provider::OpenAi),
            input_tokens: 10,
            list_cost_usd: 1.0,
            status: 200,
            ..Default::default()
         })
         .unwrap();
      }
      sample(&db, id, reset, now + 1, 48.0).await;
      let alice = report(&db, "alice", id).await;
      close(alice.estimated_user_percent, 4.0);
      assert_eq!(alice.allowance_used_percent, Some(100.0));
      assert!(
         db.user_quota_retry_after("alice", id)
            .await
            .unwrap()
            .unwrap()
            >= 1
      );
      assert_eq!(db.user_quota_retry_after("bob", id).await.unwrap(), None);
      db.flush().await.unwrap();
      drop(db);
      let db = Db::open(&path).unwrap();
      close(report(&db, "alice", id).await.estimated_user_percent, 4.0);
      assert_eq!(report(&db, "alice", id).await.budget_percent, Some(4.0));
      db.call(move |conn| {
         assert_eq!(retry_after(conn, "alice", id, reset)?, None);
         let expired = reports(conn, "alice", Some(id), reset)?.remove(0);
         assert_eq!(expired.observed_used_percent, None);
         assert_eq!(expired.resets_at, None);
         close(expired.estimated_user_percent, 0.0);
         Ok(())
      })
      .await
      .unwrap();
      db.set_user_quota_budget("alice", id, 18000, None)
         .await
         .unwrap();
      assert_eq!(db.user_quota_retry_after("alice", id).await.unwrap(), None);
      db.set_user_quota_budget("alice", id, 604_800, Some(0.0))
         .await
         .unwrap();
      assert_eq!(
         db.user_quota_retry_after("alice", id).await.unwrap(),
         Some(60)
      );
      db.call(move |conn| {
         assert_eq!(retry_after(conn, "alice", id, reset + 604_800)?, Some(60));
         Ok(())
      })
      .await
      .unwrap();
   }

   #[tokio::test]
   async fn weekly_and_short_window_have_independent_cursors_and_budgets() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let now = clock::unix_now();
      sample(&db, id, now + 18000, now, 20.0).await;
      db.observe_quota(QuotaObservation {
         account_id: id,
         window_seconds: 604_800,
         resets_at: now + 604_800,
         used_percent: 50.0,
         observed_at: now,
      })
      .await
      .unwrap();
      work(&db, id, "alice", 1.0, 10, 0).await;
      sample(&db, id, now + 18000, now + 1, 24.0).await;
      db.observe_quota(QuotaObservation {
         account_id: id,
         window_seconds: 604_800,
         resets_at: now + 604_800,
         used_percent: 51.0,
         observed_at: now + 1,
      })
      .await
      .unwrap();
      db.set_user_quota_budget("alice", id, 18000, Some(4.0))
         .await
         .unwrap();
      db.set_user_quota_budget("alice", id, 604_800, Some(1.0))
         .await
         .unwrap();
      db.call(move |conn| {
         let both = reports(conn, "alice", Some(id), now)?;
         assert_eq!(both.len(), 2);
         close(both[0].estimated_user_percent, 4.0);
         close(both[1].estimated_user_percent, 1.0);
         assert_eq!(retry_after(conn, "alice", id, now)?, Some(604_800));
         Ok(())
      })
      .await
      .unwrap();
   }

   #[tokio::test]
   async fn observations_do_not_consume_work_settled_after_poll_started() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let now = clock::unix_now();
      sample(&db, id, now + 18000, now, 0.0).await;
      work(&db, id, "alice", 1.0, 10, 0).await;
      db.call(move |conn| {
         conn.execute("UPDATE usage_log SET ts = ?1", [now + 2])?;
         Ok(())
      })
      .await
      .unwrap();
      sample(&db, id, now + 18000, now + 1, 1.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 0.0);
      sample(&db, id, now + 18000, now + 3, 2.0).await;
      close(report(&db, "alice", id).await.estimated_user_percent, 1.0);
   }

   #[tokio::test]
   async fn atomic_budget_replacement_validates_every_value_before_writing() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      db.set_user_quota_budgets("alice", id, Some(10.0), Some(20.0))
         .await
         .unwrap();
      assert!(
         db.set_user_quota_budgets("alice", id, Some(90.0), Some(f64::NAN))
            .await
            .is_err()
      );
      let rows = db.user_quota("alice", Some(id)).await.unwrap();
      assert_eq!(rows[0].budget_percent, Some(10.0));
      assert_eq!(rows[1].budget_percent, Some(20.0));
      assert!(rows.iter().all(|row| row.allowance_used_percent.is_none()));
      // Even an unexpected SQL failure during the second mutation rolls back
      // the first, rather than leaving half of a user's policy replaced.
      db.call(|conn| {
         conn.execute_batch(
            "CREATE TRIGGER fail_weekly BEFORE INSERT ON user_quota_budgets
            WHEN NEW.window_seconds = 604800 BEGIN SELECT RAISE(ABORT, 'test failure'); END;",
         )?;
         Ok(())
      })
      .await
      .unwrap();
      assert!(
         db.set_user_quota_budgets("alice", id, Some(90.0), Some(80.0))
            .await
            .is_err()
      );
      let rows = db.user_quota("alice", Some(id)).await.unwrap();
      assert_eq!(rows[0].budget_percent, Some(10.0));
      assert_eq!(rows[1].budget_percent, Some(20.0));
      db.set_user_quota_budgets("alice", id, None, None)
         .await
         .unwrap();
      assert!(db.user_quota("alice", Some(id)).await.unwrap().is_empty());
   }

   #[tokio::test]
   async fn explicit_account_removal_cascades_quota_data() {
      let (db, _) = database();
      let id = account(&db, "openai").await;
      let now = clock::unix_now();
      sample(&db, id, now + 18000, now, 44.0).await;
      db.set_user_quota_budget("alice", id, 18000, Some(10.0))
         .await
         .unwrap();
      assert_eq!(db.remove_account(&id.to_string()).await.unwrap(), 1);
      db.call(move |conn| {
         for table in [
            "quota_epochs",
            "quota_observations",
            "user_quota_estimates",
            "user_quota_budgets",
         ] {
            let count: i64 =
               conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                  row.get(0)
               })?;
            assert_eq!(count, 0);
         }
         Ok(())
      })
      .await
      .unwrap();
   }
}
