mod card;
mod config;
mod discord;
mod github;
mod issue;
mod llm;
#[cfg(test)]
mod testutil;
mod transcript;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::routing::get;
use serenity::all::{Client, GatewayIntents};

use crate::config::Config;
use crate::discord::{BotState, Handler};

const USER_AGENT: &str = concat!("discord-issue-bot/", env!("CARGO_PKG_VERSION"));

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "discord_issue_bot=info,serenity=warn".into()),
        )
        .init();

    let cfg = Arc::new(Config::from_env()?);

    // Cloud Run will not consider the container started until something is
    // listening on $PORT, so this goes up before the gateway connection.
    let health = tokio::spawn(serve_health(cfg.port));

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(USER_AGENT)
        .build()
        .context("building the HTTP client")?;

    let state = Arc::new(BotState::new(cfg.clone(), http));
    let intents = GatewayIntents::GUILDS | GatewayIntents::GUILD_MESSAGES | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&cfg.discord_token, intents)
        .event_handler(Handler::new(state))
        .await
        .context("connecting to Discord")?;

    let shard_manager = client.shard_manager.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            tracing::info!("shutting down");
            shard_manager.shutdown_all().await;
        }
    });

    tokio::select! {
        res = client.start() => res.context("the Discord client stopped")?,
        res = health => res.context("the health server panicked")?.context("the health server stopped")?,
    }
    Ok(())
}

async fn serve_health(port: u16) -> Result<()> {
    let app = Router::new()
        .route("/", get(|| async { "ok" }))
        .route("/healthz", get(|| async { "ok" }));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("binding port {port}"))?;
    tracing::info!(port, "health endpoint listening");
    axum::serve(listener, app).await.context("serving health")
}
