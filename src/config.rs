use std::collections::HashMap;
use std::str::FromStr;

use anyhow::{Context as _, Result, anyhow, bail};

/// A GitHub repository the bot can file issues into.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl FromStr for Repo {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let s = s
            .trim()
            .trim_start_matches("https://github.com/")
            .trim_end_matches('/');
        let (owner, name) = s
            .split_once('/')
            .ok_or_else(|| anyhow!("expected `owner/repo`, got `{s}`"))?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            bail!("expected `owner/repo`, got `{s}`");
        }
        Ok(Repo {
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
}

impl std::fmt::Display for Repo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

/// Every bound on how much thread content we are willing to handle.
///
/// These exist so that a thread that is long by accident (a busy support
/// channel) or long on purpose (someone pasting a megabyte of text to burn
/// tokens or to smuggle instructions past the model) costs a bounded amount.
#[derive(Clone, Debug)]
pub struct Limits {
    /// Hard cap on how many thread messages we fetch from Discord.
    pub max_messages: usize,
    /// Per-message character cap before it is elided.
    pub max_message_chars: usize,
    /// Total characters of transcript handed to the LLM.
    pub max_llm_chars: usize,
    /// Total characters of the rendered GitHub issue body (GitHub's own cap is 65536).
    pub max_body_chars: usize,
    /// How many attachments we carry over per thread.
    pub max_attachments: usize,
    /// Minimum seconds between two requests from the same user.
    pub user_cooldown_secs: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_messages: 200,
            max_message_chars: 2_000,
            max_llm_chars: 24_000,
            max_body_chars: 60_000,
            max_attachments: 20,
            user_cooldown_secs: 20,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub discord_token: String,
    pub openai_api_key: String,
    pub openai_model: String,
    pub openai_base_url: String,
    pub github_token: String,
    /// Fallback repo, used for any guild without an explicit mapping.
    pub default_repo: Option<Repo>,
    /// Per-guild overrides, keyed by Discord guild id.
    pub guild_repos: HashMap<u64, Repo>,
    /// If non-empty, a user must hold one of these roles to file an issue.
    pub allowed_role_ids: Vec<u64>,
    pub limits: Limits,
    pub port: u16,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let limits = Limits::default();
        let cfg = Config {
            discord_token: require("DISCORD_TOKEN")?,
            openai_api_key: require("OPENAI_API_KEY")?,
            openai_model: optional("OPENAI_MODEL").unwrap_or_else(|| "gpt-4o-mini".to_owned()),
            openai_base_url: optional("OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_owned())
                .trim_end_matches('/')
                .to_owned(),
            github_token: require("GITHUB_TOKEN")?,
            default_repo: optional("GITHUB_REPO")
                .map(|v| v.parse::<Repo>().context("GITHUB_REPO"))
                .transpose()?,
            guild_repos: parse_guild_repos(optional("GITHUB_REPO_MAP").as_deref())?,
            allowed_role_ids: parse_id_list(optional("ALLOWED_ROLE_IDS").as_deref())
                .context("ALLOWED_ROLE_IDS")?,
            limits: Limits {
                max_messages: num("MAX_THREAD_MESSAGES", limits.max_messages)?,
                max_message_chars: num("MAX_MESSAGE_CHARS", limits.max_message_chars)?,
                max_llm_chars: num("MAX_LLM_CHARS", limits.max_llm_chars)?,
                max_body_chars: num("MAX_BODY_CHARS", limits.max_body_chars)?.min(65_000),
                max_attachments: num("MAX_ATTACHMENTS", limits.max_attachments)?,
                user_cooldown_secs: num("USER_COOLDOWN_SECS", limits.user_cooldown_secs)?,
            },
            port: num("PORT", 8080u16)?,
        };

        if cfg.default_repo.is_none() && cfg.guild_repos.is_empty() {
            bail!(
                "set GITHUB_REPO (e.g. `owner/repo`) or GITHUB_REPO_MAP, otherwise the bot has nowhere to file issues"
            );
        }
        Ok(cfg)
    }

    /// The repo a given guild files into: its own mapping, else the default.
    pub fn repo_for_guild(&self, guild_id: u64) -> Option<&Repo> {
        self.guild_repos.get(&guild_id).or(self.default_repo.as_ref())
    }
}

/// `{"123456789": "owner/repo"}` — lets one deployment serve several servers.
fn parse_guild_repos(raw: Option<&str>) -> Result<HashMap<u64, Repo>> {
    let Some(raw) = raw else { return Ok(HashMap::new()) };
    let parsed: HashMap<String, String> = serde_json::from_str(raw)
        .context("GITHUB_REPO_MAP must be JSON like {\"<guild id>\": \"owner/repo\"}")?;
    parsed
        .into_iter()
        .map(|(guild, repo)| {
            let guild: u64 = guild
                .parse()
                .with_context(|| format!("GITHUB_REPO_MAP key `{guild}` is not a guild id"))?;
            Ok((guild, repo.parse()?))
        })
        .collect()
}

fn parse_id_list(raw: Option<&str>) -> Result<Vec<u64>> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().with_context(|| format!("`{s}` is not an id")))
        .collect()
}

fn optional(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn require(key: &str) -> Result<String> {
    optional(key).ok_or_else(|| anyhow!("{key} is required but unset"))
}

fn num<T>(key: &str, default: T) -> Result<T>
where
    T: FromStr,
    <T as FromStr>::Err: std::fmt::Display,
{
    match optional(key) {
        None => Ok(default),
        Some(v) => v.parse::<T>().map_err(|e| anyhow!("{key}: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_forms() {
        assert_eq!("a/b".parse::<Repo>().unwrap().to_string(), "a/b");
        assert_eq!(
            "https://github.com/a/b".parse::<Repo>().unwrap().to_string(),
            "a/b"
        );
        assert!("nope".parse::<Repo>().is_err());
        assert!("a/b/c".parse::<Repo>().is_err());
        assert!("/b".parse::<Repo>().is_err());
    }

    #[test]
    fn guild_map_falls_back_to_default() {
        let cfg = Config {
            discord_token: String::new(),
            openai_api_key: String::new(),
            openai_model: String::new(),
            openai_base_url: String::new(),
            github_token: String::new(),
            default_repo: Some("d/efault".parse().unwrap()),
            guild_repos: parse_guild_repos(Some(r#"{"42": "o/verride"}"#)).unwrap(),
            allowed_role_ids: vec![],
            limits: Limits::default(),
            port: 8080,
        };
        assert_eq!(cfg.repo_for_guild(42).unwrap().to_string(), "o/verride");
        assert_eq!(cfg.repo_for_guild(7).unwrap().to_string(), "d/efault");
    }

    #[test]
    fn parses_role_lists() {
        assert_eq!(parse_id_list(Some("1, 2,3")).unwrap(), vec![1, 2, 3]);
        assert!(parse_id_list(None).unwrap().is_empty());
        assert!(parse_id_list(Some("abc")).is_err());
    }
}
