use super::*;
use crate::db::quota::QuotaObservation;
use std::time::Duration;

async fn spawn_proxy_with_codex_status(status: StatusCode) -> (String, Db) {
   let app = Router::new().route(
      "/responses",
      post(
         move || async move { (status, [("retry-after", "7")], "quota exhausted").into_response() },
      ),
   );
   let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
   let upstream_addr = upstream_listener.local_addr().unwrap();
   tokio::spawn(async move {
      axum::serve(upstream_listener, app).await.unwrap();
   });
   let base_url = format!("http://{upstream_addr}");
   let db_path = env::temp_dir().join(format!("slop-test-{}.db", uuid::Uuid::new_v4()));
   let db = Db::open(&db_path).unwrap();
   db.create_token("alice", "sp-test", "sp-test")
      .await
      .unwrap();
   db.upsert_account(NewAccount {
      provider: Provider::OpenAi,
      id: "acct-1",
      email: Some("test@example.com"),
      label: None,
      plan: Some("plus"),
      tokens: &fresh_tokens(),
      auth_mode: AuthMode::OAuth,
   })
   .await
   .unwrap();
   let cfg_db_path = db_path.clone();
   let cfg = Config {
      db_path,
      bind: String::new(),
      metrics_bind: None,
      codex: CodexConfig {
         base_url,
         ..CodexConfig::default()
      },
      anthropic: AnthropicConfig::default(),
      gemini: GeminiConfig::default(),
      zen: ZenConfig::default(),
      glm: GlmConfig::default(),
      experiential: ExperientialConfig::default(),
      pricing: PricingConfig::default(),
      models: ModelsConfig {
         anthropic_patterns: Vec::new(),
         ..ModelsConfig::default()
      },
   };
   let pools = Pools::load(&db, &cfg).await.unwrap();
   let state = AppState(Arc::new(Inner {
      db: db.clone(),
      cfg,
      prices: Prices::new(&cfg_db_path, PricingConfig::default().url),
      pools,
   }));
   let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
   let proxy_addr = proxy_listener.local_addr().unwrap();
   tokio::spawn(async move {
      axum::serve(proxy_listener, router(state)).await.unwrap();
   });
   (format!("http://{proxy_addr}"), db)
}

#[tokio::test]
async fn retry_after_on_request_limit_for_both_dialects() {
   let (base, db) = spawn_proxy().await;
   let id = db
      .create_token("retry-one", "sp-retry-one", "sp-retry-one")
      .await
      .unwrap();
   db.set_token_limits(
      &id.to_string(),
      &TokenLimits {
         providers: Vec::new(),
         requests: Some(1),
         tokens: None,
         window_seconds: 3600,
         slowdown_ms: 0,
         prefer_trusted: false,
         five_hour_limit: None,
         weekly_limit: None,
         pinned_account: None,
      },
   )
   .await
   .unwrap();
   let client = reqwest::Client::new();
   let chat = serde_json::json!({
      "model": "gpt-5-codex",
      "messages": [{"role": "user", "content": "hi"}]
   });
   let first = client
      .post(format!("{base}/v1/chat/completions"))
      .bearer_auth("sp-retry-one")
      .json(&chat)
      .send()
      .await
      .unwrap();
   assert_eq!(first.status(), 200);
   assert_eq!(
      first.headers().get("x-ratelimit-limit-requests").unwrap(),
      "1"
   );
   assert_eq!(
      first
         .headers()
         .get("x-ratelimit-remaining-requests")
         .unwrap(),
      "0"
   );
   first.bytes().await.unwrap();
   let second = client
      .post(format!("{base}/v1/chat/completions"))
      .bearer_auth("sp-retry-one")
      .json(&chat)
      .send()
      .await
      .unwrap();
   assert_eq!(second.status(), 429);
   let retry = second
      .headers()
      .get("retry-after")
      .unwrap()
      .to_str()
      .unwrap()
      .parse::<i64>()
      .unwrap();
   assert!(retry >= 1);
   let value: serde_json::Value = second.json().await.unwrap();
   assert_eq!(value["error"]["type"], "rate_limit_error");
   let third = client
      .post(format!("{base}/v1/messages"))
      .header("x-api-key", "sp-retry-one")
      .json(&serde_json::json!({
         "model": "claude-sonnet-4",
         "max_tokens": 10_u64,
         "messages": [{"role": "user", "content": "hi"}]
      }))
      .send()
      .await
      .unwrap();
   assert_eq!(third.status(), 429);
   let retry_third = third
      .headers()
      .get("retry-after")
      .unwrap()
      .to_str()
      .unwrap()
      .parse::<i64>()
      .unwrap();
   assert!(retry_third >= 1);
   let third_value: serde_json::Value = third.json().await.unwrap();
   assert_eq!(third_value["type"], "error");
   assert_eq!(third_value["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn retry_after_on_token_limit() {
   let (base, db) = spawn_proxy().await;
   let id = db
      .create_token("retry-two", "sp-retry-two", "sp-retry-two")
      .await
      .unwrap();
   db.set_token_limits(
      &id.to_string(),
      &TokenLimits {
         providers: Vec::new(),
         requests: None,
         tokens: Some(10),
         window_seconds: 3600,
         slowdown_ms: 0,
         prefer_trusted: false,
         five_hour_limit: None,
         weekly_limit: None,
         pinned_account: None,
      },
   )
   .await
   .unwrap();
   let client = reqwest::Client::new();
   let chat = serde_json::json!({
      "model": "gpt-5-codex",
      "messages": [{"role": "user", "content": "hi"}]
   });
   let first = client
      .post(format!("{base}/v1/chat/completions"))
      .bearer_auth("sp-retry-two")
      .json(&chat)
      .send()
      .await
      .unwrap();
   assert_eq!(first.status(), 200);
   first.bytes().await.unwrap();
   db.flush().await.unwrap();
   let second = client
      .post(format!("{base}/v1/chat/completions"))
      .bearer_auth("sp-retry-two")
      .json(&chat)
      .send()
      .await
      .unwrap();
   assert_eq!(second.status(), 429);
   let retry = second
      .headers()
      .get("retry-after")
      .unwrap()
      .to_str()
      .unwrap()
      .parse::<i64>()
      .unwrap();
   assert!(retry >= 1);
   let value: serde_json::Value = second.json().await.unwrap();
   assert_eq!(value["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn retry_after_on_codex_upstream_rate_limit() {
   let (base, _db) = spawn_proxy_with_codex_status(StatusCode::TOO_MANY_REQUESTS).await;
   let response = reqwest::Client::new()
      .post(format!("{base}/v1/chat/completions"))
      .bearer_auth("sp-test")
      .json(&serde_json::json!({
         "model": "gpt-5-codex",
         "messages": [{"role": "user", "content": "hi"}]
      }))
      .timeout(Duration::from_secs(5))
      .send()
      .await
      .unwrap();
   assert_eq!(response.status(), 429);
   let retry = response
      .headers()
      .get("retry-after")
      .unwrap()
      .to_str()
      .unwrap()
      .parse::<i64>()
      .unwrap();
   assert!(retry >= 1);
   let value: serde_json::Value = response.json().await.unwrap();
   assert_eq!(value["error"]["type"], "rate_limit_exceeded");
}

#[tokio::test]
async fn retry_after_on_anthropic_upstream_rate_limit() {
   let app = Router::new().route(
      "/v1/messages",
      post(|| async {
         (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "7")],
            "slow down",
         )
            .into_response()
      }),
   );
   let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
   let upstream = format!("http://{}", listener.local_addr().unwrap());
   tokio::spawn(async move {
      axum::serve(listener, app).await.unwrap();
   });
   let (base, _db) = spawn_proxy_with(ModelsConfig::default(), Some(upstream)).await;
   let response = reqwest::Client::new()
      .post(format!("{base}/v1/messages"))
      .header("x-api-key", "sp-test")
      .header("anthropic-beta", "claude-code-20250219")
      .header("user-agent", "claude-cli/2.1.252 (external, cli)")
      .json(&serde_json::json!({
         "model": "claude-opus-5",
         "max_tokens": 10_u64,
         "messages": [{"role": "user", "content": "hi"}]
      }))
      .timeout(Duration::from_secs(5))
      .send()
      .await
      .unwrap();
   assert_eq!(response.status(), 429);
   let retry = response
      .headers()
      .get("retry-after")
      .unwrap()
      .to_str()
      .unwrap()
      .parse::<i64>()
      .unwrap();
   assert!(retry >= 1);
   let value: serde_json::Value = response.json().await.unwrap();
   assert_eq!(value["type"], "error");
   assert_eq!(value["error"]["type"], "rate_limit_error");
}

#[tokio::test]
async fn rate_limit_headers_present_on_success() {
   let (base, db) = spawn_proxy().await;
   let id = db
      .create_token("retry-three", "sp-retry-three", "sp-retry-three")
      .await
      .unwrap();
   db.set_token_limits(
      &id.to_string(),
      &TokenLimits {
         providers: Vec::new(),
         requests: Some(5),
         tokens: Some(100_000),
         window_seconds: 3600,
         slowdown_ms: 0,
         prefer_trusted: false,
         five_hour_limit: None,
         weekly_limit: None,
         pinned_account: None,
      },
   )
   .await
   .unwrap();
   let response = reqwest::Client::new()
      .post(format!("{base}/v1/chat/completions"))
      .bearer_auth("sp-retry-three")
      .json(&serde_json::json!({
         "model": "gpt-5-codex",
         "messages": [{"role": "user", "content": "hi"}]
      }))
      .send()
      .await
      .unwrap();
   assert_eq!(response.status(), 200);
   assert_eq!(
      response
         .headers()
         .get("x-ratelimit-limit-requests")
         .unwrap(),
      "5"
   );
   assert_eq!(
      response
         .headers()
         .get("x-ratelimit-remaining-requests")
         .unwrap(),
      "4"
   );
   assert_eq!(
      response.headers().get("x-ratelimit-limit-tokens").unwrap(),
      "100000"
   );
   assert!(
      response
         .headers()
         .get("x-ratelimit-reset")
         .unwrap()
         .to_str()
         .unwrap()
         .parse::<i64>()
         .unwrap()
         >= 1
   );
   response.bytes().await.unwrap();
}

#[tokio::test]
async fn estimated_usd_budget_returns_429_before_upstream() {
   // The upstream would return 400 if reached, so 429 proves local admission.
   let (base, db) = spawn_proxy_with_codex_status(StatusCode::BAD_REQUEST).await;
   let account = db.list_accounts().await.unwrap().remove(0).id;
   db.set_user_spend_budget("alice", account, Some(0.0_f64))
      .await
      .unwrap();
   let response = reqwest::Client::new()
      .post(format!("{base}/v1/chat/completions"))
      .bearer_auth("sp-test")
      .json(&serde_json::json!({
         "model": "gpt-5-codex",
         "messages": [{"role": "user", "content": "hi"}]
      }))
      .timeout(Duration::from_secs(5))
      .send()
      .await
      .unwrap();
   assert_eq!(response.status(), 429);
   assert!(response.headers()["retry-after"].to_str().unwrap().parse::<i64>().unwrap() >= 60);
   let body: serde_json::Value = response.json().await.unwrap();
   assert!(body["error"]["message"].as_str().unwrap().contains("estimated USD budget"));
}

#[tokio::test]
async fn estimated_user_quota_returns_429_before_upstream_for_both_dialects() {
   // The upstream would return 400 if reached, so 429 proves local admission.
   let (base, db) = spawn_proxy_with_codex_status(StatusCode::BAD_REQUEST).await;
   let account = db.list_accounts().await.unwrap().remove(0).id;
   db.observe_quota(QuotaObservation {
      account_id: account,
      window_seconds: 18_000,
      resets_at: clock::unix_now() + 3600,
      used_percent: 25.0_f64,
      observed_at: clock::unix_now(),
   })
   .await
   .unwrap();
   db.set_user_quota_budget("alice", account, 18_000, Some(0.0_f64))
      .await
      .unwrap();
   let client = reqwest::Client::new();
   for path in ["chat/completions", "messages"] {
      let response = client
         .post(format!("{base}/v1/{path}"))
         .bearer_auth("sp-test")
         .json(
            &serde_json::json!({"model":"gpt-5-codex", "max_tokens":10_i32,
            "messages":[{"role":"user","content":"hi"}]}),
         )
         .timeout(Duration::from_secs(5))
         .send()
         .await
         .unwrap();
      assert_eq!(response.status(), 429);
      assert!(
         response.headers()["retry-after"]
            .to_str()
            .unwrap()
            .parse::<i64>()
            .unwrap()
            > 0
      );
      let body: serde_json::Value = response.json().await.unwrap();
      assert!(
         body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("estimated user quota budget")
      );
   }
}
