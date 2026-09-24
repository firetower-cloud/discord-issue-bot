//! The GitHub half: read the repo's labels, file the issue.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::config::Repo;

const API: &str = "https://api.github.com";
const LABEL_TTL: Duration = Duration::from_secs(600);

pub struct GitHub {
    http: reqwest::Client,
    token: String,
    base: String,
    label_cache: Mutex<HashMap<Repo, (Vec<String>, Instant)>>,
}

#[derive(Debug, Deserialize)]
pub struct CreatedIssue {
    pub number: u64,
    pub html_url: String,
}

#[derive(Debug, Deserialize)]
struct Label {
    name: String,
}

impl GitHub {
    pub fn new(http: reqwest::Client, token: String) -> Self {
        Self::with_base(http, token, API.to_owned())
    }

    fn with_base(http: reqwest::Client, token: String, base: String) -> Self {
        Self {
            http,
            token,
            base,
            label_cache: Mutex::new(HashMap::new()),
        }
    }

    fn request(&self, method: reqwest::Method, url: String) -> reqwest::RequestBuilder {
        self.http
            .request(method, url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// The repo's existing labels, so the model picks from what actually
    /// exists instead of inventing new ones. Cached, and best-effort: if
    /// GitHub is unreachable we file the issue unlabelled rather than fail.
    pub async fn labels(&self, repo: &Repo) -> Vec<String> {
        if let Ok(cache) = self.label_cache.lock()
            && let Some((labels, fetched)) = cache.get(repo)
            && fetched.elapsed() < LABEL_TTL
        {
            return labels.clone();
        }

        let labels = match self.fetch_labels(repo).await {
            Ok(labels) => labels,
            Err(e) => {
                tracing::warn!(repo = %repo, error = %e, "could not list labels; filing without one");
                return Vec::new();
            }
        };

        if let Ok(mut cache) = self.label_cache.lock() {
            cache.insert(repo.clone(), (labels.clone(), Instant::now()));
        }
        labels
    }

    async fn fetch_labels(&self, repo: &Repo) -> Result<Vec<String>> {
        let url = format!(
            "{}/repos/{}/{}/labels?per_page=100",
            self.base, repo.owner, repo.name
        );
        let res = self
            .request(reqwest::Method::GET, url)
            .send()
            .await
            .context("GET labels")?;
        let res = check(res).await?;
        let labels: Vec<Label> = res.json().await.context("decoding labels")?;
        Ok(labels.into_iter().map(|l| l.name).collect())
    }

    pub async fn create_issue(
        &self,
        repo: &Repo,
        title: &str,
        body: &str,
        labels: &[String],
    ) -> Result<CreatedIssue> {
        let url = format!("{}/repos/{}/{}/issues", self.base, repo.owner, repo.name);
        let mut payload = json!({ "title": title, "body": body });
        if !labels.is_empty() {
            payload["labels"] = json!(labels);
        }

        let res = self
            .request(reqwest::Method::POST, url)
            .json(&payload)
            .send()
            .await
            .context("POST issue")?;
        let res = check(res).await?;
        res.json().await.context("decoding the created issue")
    }
}

/// Turn a non-2xx into an error that says what GitHub actually complained about.
async fn check(res: reqwest::Response) -> Result<reqwest::Response> {
    let status = res.status();
    if status.is_success() {
        return Ok(res);
    }
    let body = res.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(str::to_owned))
        .unwrap_or_else(|| body.chars().take(300).collect());
    let hint = match status.as_u16() {
        401 => " (check GITHUB_TOKEN)",
        403 => " (the token is missing repo/issues write access, or you are rate limited)",
        404 => " (repo not found, or the token cannot see it)",
        410 => " (issues are disabled on this repo)",
        _ => "",
    };
    bail!("GitHub returned {status}{hint}: {detail}");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Repo {
        "octocat/hello".parse().unwrap()
    }

    #[tokio::test]
    async fn creates_an_issue_and_returns_its_url() {
        let body =
            json!({ "number": 12, "html_url": "https://github.com/octocat/hello/issues/12" }).to_string();
        let server = crate::testutil::spawn(201, body).await;
        let gh = GitHub::with_base(reqwest::Client::new(), "tok".into(), server.base.clone());

        let issue = gh
            .create_issue(&repo(), "Export returns 500", "the body", &["bug".to_owned()])
            .await
            .unwrap();

        assert_eq!(issue.number, 12);
        assert_eq!(issue.html_url, "https://github.com/octocat/hello/issues/12");

        let sent = server.last_request_json();
        assert_eq!(sent["title"], "Export returns 500");
        assert_eq!(sent["body"], "the body");
        assert_eq!(sent["labels"], json!(["bug"]));
    }

    #[tokio::test]
    async fn an_empty_label_set_is_left_out_of_the_payload() {
        let server = crate::testutil::spawn(201, json!({ "number": 1, "html_url": "u" }).to_string()).await;
        let gh = GitHub::with_base(reqwest::Client::new(), "tok".into(), server.base.clone());

        gh.create_issue(&repo(), "t", "b", &[]).await.unwrap();

        // Sending `"labels": []` is fine on create but pointless; assert we omit it
        // so the request says only what we mean.
        assert!(server.last_request_json().get("labels").is_none());
    }

    #[tokio::test]
    async fn permission_failures_explain_themselves() {
        let body = json!({ "message": "Resource not accessible by personal access token" }).to_string();
        let server = crate::testutil::spawn(403, body).await;
        let gh = GitHub::with_base(reqwest::Client::new(), "tok".into(), server.base.clone());

        let err = gh
            .create_issue(&repo(), "t", "b", &[])
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("403"), "{err}");
        assert!(err.contains("issues write access"), "{err}");
        assert!(err.contains("Resource not accessible"), "{err}");
    }

    #[tokio::test]
    async fn listing_labels_is_best_effort() {
        let server = crate::testutil::spawn(500, "boom").await;
        let gh = GitHub::with_base(reqwest::Client::new(), "tok".into(), server.base.clone());

        // A label lookup that fails must not stop an issue being filed.
        assert!(gh.labels(&repo()).await.is_empty());
    }

    #[tokio::test]
    async fn labels_are_fetched_once_then_cached() {
        let body = json!([{ "name": "bug" }, { "name": "enhancement" }]).to_string();
        let server = crate::testutil::spawn(200, body).await;
        let gh = GitHub::with_base(reqwest::Client::new(), "tok".into(), server.base.clone());

        assert_eq!(gh.labels(&repo()).await, vec!["bug", "enhancement"]);
        assert_eq!(gh.labels(&repo()).await, vec!["bug", "enhancement"]);
        assert_eq!(
            server.requests().len(),
            1,
            "the second call must come from the cache"
        );
    }
}
