//! A throwaway HTTP server so the OpenAI and GitHub clients can be exercised
//! for real — request shape included — without network access.

use std::sync::{Arc, Mutex};

use axum::Router;
use axum::http::StatusCode;

use crate::config::{Config, Limits};

pub fn test_config(openai_base: &str) -> Config {
    Config {
        discord_token: "t".into(),
        openai_api_key: "sk-test".into(),
        openai_model: "gpt-4o-mini".into(),
        openai_base_url: openai_base.to_owned(),
        github_token: "g".into(),
        default_repo: Some("octocat/hello".parse().unwrap()),
        guild_repos: Default::default(),
        allowed_role_ids: vec![],
        limits: Limits::default(),
        port: 0,
    }
}

pub struct MockServer {
    pub base: String,
    requests: Arc<Mutex<Vec<String>>>,
}

impl MockServer {
    /// Bodies of every request the server received, in order.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    pub fn last_request_json(&self) -> serde_json::Value {
        let reqs = self.requests();
        serde_json::from_str(reqs.last().expect("no request was made")).expect("request body was not JSON")
    }
}

/// Replies to every request with `status` and `body`.
pub async fn spawn(status: u16, body: impl Into<String>) -> MockServer {
    let body = body.into();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = requests.clone();

    let app = Router::new().fallback(move |req_body: String| {
        let body = body.clone();
        let seen = seen.clone();
        async move {
            seen.lock().unwrap().push(req_body);
            (StatusCode::from_u16(status).unwrap(), body)
        }
    });

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    MockServer {
        base: format!("http://{addr}"),
        requests,
    }
}
