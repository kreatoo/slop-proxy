use axum::Json;
use axum::extract::{Extension, State};
use axum::response::{IntoResponse as _, Response};
use serde::Serialize;

use super::AppState;
use super::auth::AuthInfo;
use super::error::{Dialect, error_response};

#[derive(Debug, Serialize)]
pub struct UsageResponse {
   pub user: String,
   pub fleet: FleetUsage,
}

#[derive(Debug, Serialize)]
pub struct FleetUsage {
   pub windows: Vec<FleetWindow>,
   pub plan_weights: PlanWeights,
   pub personal_accounts_excluded: bool,
}

#[derive(Debug, Serialize)]
pub struct PlanWeights {
   pub plus: f64,
   pub prolite: f64,
   pub pro: f64,
}

#[derive(Debug, Serialize)]
pub struct FleetWindow {
   pub window_seconds: i64,
   pub reset_at: Option<i64>,
   pub budget_percent: Option<f64>,
   pub weighted_capacity_points: f64,
   pub weighted_allowance_points: Option<f64>,
   pub weighted_used_points: f64,
   pub weighted_remaining_points: Option<f64>,
   pub allowance_used_percent: Option<f64>,
   pub allowance_remaining_percent: Option<f64>,
}

pub async fn usage(
   State(state): State<AppState>,
   Extension(auth): Extension<AuthInfo>,
) -> Response {
   let reports = match state.db.user_quota(&auth.user, None).await {
      Ok(reports) => reports,
      Err(err) => {
         tracing::error!(user = %auth.user, "reading usage report failed: {err}");
         return error_response(Dialect::OpenAi, 500, "api_error", "internal error");
      },
   };

   let windows = reports
      .into_iter()
      .filter(|report| report.account_id.is_none())
      .map(|report| {
         let capacity = report.fleet_capacity_points.unwrap_or(0.0_f64);
         let used = report.fleet_estimated_user_percent.unwrap_or(0.0_f64);
         let allowance = report
            .budget_percent
            .map(|budget| capacity * budget / 100.0_f64);
         let remaining = allowance.map(|value| (value - used).max(0.0_f64));
         let used_percent = report.fleet_allowance_used_percent;
         FleetWindow {
            window_seconds: report.window_seconds,
            reset_at: report.resets_at,
            budget_percent: report.budget_percent,
            weighted_capacity_points: capacity,
            weighted_allowance_points: allowance,
            weighted_used_points: used,
            weighted_remaining_points: remaining,
            allowance_used_percent: used_percent,
            allowance_remaining_percent: used_percent.map(|value| (100.0_f64 - value).max(0.0_f64)),
         }
      })
      .collect();

   Json(UsageResponse {
      user: auth.user,
      fleet: FleetUsage {
         windows,
         plan_weights: PlanWeights {
            plus: 1.0,
            prolite: 5.0,
            pro: 20.0,
         },
         personal_accounts_excluded: true,
      },
   })
   .into_response()
}
