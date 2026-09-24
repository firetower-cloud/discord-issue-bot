//! The Discord half: spot `@issue` in a thread, propose a draft, and file it
//! once someone confirms.
//!
//! The draft lives between two events — the message that triggers it and the
//! button that confirms it — so `BotState::pending` holds it in between, keyed
//! by the card's message id.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use serenity::all::*;
use serenity::async_trait;

use crate::card::{self, PendingDraft};
use crate::config::Config;
use crate::github::{CreatedIssue, GitHub};
use crate::issue::{IssueContext, render_body};
use crate::llm;
use crate::transcript::{self, Asset, ThreadMessage};

/// Typed as a plain word so `@issue` also works for people who type it out
/// instead of picking the bot from Discord's autocomplete.
const TEXT_TRIGGER: &str = "@issue";

pub struct BotState {
    pub cfg: Arc<Config>,
    pub github: GitHub,
    pub http: reqwest::Client,
    cooldowns: Mutex<HashMap<UserId, Instant>>,
    /// Threads already filed, so a second `@issue` links the issue instead of
    /// opening a duplicate. In-memory: a restart forgets them.
    filed: Mutex<HashMap<ChannelId, String>>,
    /// Draft cards waiting on a confirmation, keyed by the card's message id.
    pending: Mutex<HashMap<MessageId, PendingDraft>>,
    in_flight: Mutex<HashSet<ChannelId>>,
}

impl BotState {
    pub fn new(cfg: Arc<Config>, http: reqwest::Client) -> Self {
        let github = GitHub::new(http.clone(), cfg.github_token.clone());
        Self {
            cfg,
            github,
            http,
            cooldowns: Mutex::new(HashMap::new()),
            filed: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            in_flight: Mutex::new(HashSet::new()),
        }
    }

    /// Returns the wait remaining, or None if the user may proceed.
    fn check_cooldown(&self, user: UserId) -> Option<Duration> {
        let window = Duration::from_secs(self.cfg.limits.user_cooldown_secs);
        let mut cooldowns = self.cooldowns.lock().ok()?;
        if cooldowns.len() > 1_000 {
            cooldowns.retain(|_, last| last.elapsed() < window);
        }
        if let Some(last) = cooldowns.get(&user)
            && last.elapsed() < window
        {
            return Some(window - last.elapsed());
        }
        cooldowns.insert(user, Instant::now());
        None
    }
}

/// Released on drop so a failed run doesn't wedge the thread forever.
struct InFlight(Arc<BotState>, ChannelId);

impl Drop for InFlight {
    fn drop(&mut self) {
        if let Ok(mut set) = self.0.in_flight.lock() {
            set.remove(&self.1);
        }
    }
}

pub struct Handler {
    state: Arc<BotState>,
}

impl Handler {
    pub fn new(state: Arc<BotState>) -> Self {
        Self { state }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        tracing::info!(
            bot = %ready.user.name,
            guilds = ready.guilds.len(),
            "connected to Discord; mention me in a thread to draft an issue"
        );
    }

    async fn message(&self, ctx: Context, msg: Message) {
        if msg.author.bot {
            return;
        }
        let Some(guild_id) = msg.guild_id else { return };

        let mentioned = msg.mentions_me(&ctx).await.unwrap_or(false);
        if !mentioned && !msg.content.to_lowercase().contains(TEXT_TRIGGER) {
            return;
        }

        let _ = msg.react(&ctx.http, '👀').await;
        match propose(self.state.clone(), &ctx, &msg, guild_id).await {
            // The card itself is the reply.
            Ok(Outcome::Proposed) => {}
            Ok(Outcome::AlreadyFiled(url)) => {
                reply(&ctx, &msg, &format!("This thread already has an issue: {url}")).await;
            }
            Ok(Outcome::Ignored) => {}
            Err(e) => {
                tracing::warn!(channel = %msg.channel_id, error = ?e, "could not draft an issue");
                let _ = msg.react(&ctx.http, '⚠').await;
                reply(&ctx, &msg, &format!("Could not draft the issue: {e}")).await;
            }
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let outcome = match &interaction {
            Interaction::Component(component) => on_component(self.state.clone(), &ctx, component).await,
            Interaction::Modal(modal) => on_modal(self.state.clone(), &ctx, modal).await,
            _ => return,
        };
        if let Err(e) = outcome {
            tracing::warn!(error = ?e, "could not handle an interaction");
        }
    }
}

enum Outcome {
    /// A card is up and waiting on a button.
    Proposed,
    AlreadyFiled(String),
    /// Triggered somewhere it does not apply; stay quiet rather than nag.
    Ignored,
}

/// Read the thread, draft an issue, and post it as a card. Files nothing.
async fn propose(state: Arc<BotState>, ctx: &Context, msg: &Message, guild_id: GuildId) -> Result<Outcome> {
    let Channel::Guild(thread) = msg
        .channel_id
        .to_channel(ctx)
        .await
        .context("looking up this channel")?
    else {
        return Ok(Outcome::Ignored);
    };
    if !matches!(
        thread.kind,
        ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
    ) {
        reply(
            ctx,
            msg,
            "Mention me **inside a thread** and I'll turn it into a GitHub issue.",
        )
        .await;
        return Ok(Outcome::Ignored);
    }

    if !state.cfg.allowed_role_ids.is_empty() {
        let roles = msg.member.as_ref().map(|m| m.roles.as_slice()).unwrap_or(&[]);
        if !roles
            .iter()
            .any(|r| state.cfg.allowed_role_ids.contains(&r.get()))
        {
            bail!("you don't have a role that's allowed to file issues here");
        }
    }

    let repo = state
        .cfg
        .repo_for_guild(guild_id.get())
        .ok_or_else(|| anyhow!("no GitHub repo is configured for this server"))?
        .clone();

    if let Ok(filed) = state.filed.lock()
        && let Some(url) = filed.get(&msg.channel_id)
    {
        return Ok(Outcome::AlreadyFiled(url.clone()));
    }

    if let Ok(pending) = state.pending.lock()
        && pending.values().any(|p| p.channel_id == msg.channel_id)
    {
        bail!("there's already a draft waiting in this thread — confirm or cancel it first");
    }

    if let Some(wait) = state.check_cooldown(msg.author.id) {
        bail!("slow down a moment — try again in {}s", wait.as_secs() + 1);
    }

    {
        let mut set = state
            .in_flight
            .lock()
            .map_err(|_| anyhow!("internal lock poisoned"))?;
        if !set.insert(msg.channel_id) {
            return Ok(Outcome::Ignored);
        }
    }
    let _guard = InFlight(state.clone(), msg.channel_id);

    let messages = collect_thread(ctx, &thread, msg.id, state.cfg.limits.max_messages).await?;
    if messages.is_empty() {
        bail!("I couldn't read any messages in this thread");
    }
    let first_content = messages[0].content.clone();
    let script = transcript::build(&messages, &state.cfg.limits);

    let available_labels = state.github.labels(&repo).await;
    let draft = match llm::draft(&state.http, &state.cfg, &script, &available_labels).await {
        Ok(draft) => draft,
        Err(e) => {
            tracing::warn!(error = %e, "the drafting step failed; proposing the raw thread");
            llm::fallback(&first_content)
        }
    };

    let pending = PendingDraft {
        channel_id: msg.channel_id,
        requester: msg.author.id,
        repo,
        draft,
        available_labels,
        transcript: script,
        thread_url: format!(
            "https://discord.com/channels/{}/{}",
            guild_id.get(),
            thread.id.get()
        ),
        reporter: display_name(&msg.author),
    };

    let card = msg
        .channel_id
        .send_message(
            &ctx.http,
            CreateMessage::new()
                .embed(card::embed(&pending))
                .components(card::components(&pending))
                .reference_message(msg),
        )
        .await
        .context("posting the draft card")?;

    state
        .pending
        .lock()
        .map_err(|_| anyhow!("internal lock poisoned"))?
        .insert(card.id, pending);

    Ok(Outcome::Proposed)
}

/// Button presses and label picks on a card.
async fn on_component(state: Arc<BotState>, ctx: &Context, component: &ComponentInteraction) -> Result<()> {
    let id = component.data.custom_id.as_str();
    if ![card::CREATE, card::EDIT, card::CANCEL, card::LABELS].contains(&id) {
        return Ok(());
    }

    let Some(pending) = lookup(&state, component.message.id)? else {
        return expired(ctx, component).await;
    };
    if component.user.id != pending.requester {
        return refuse(ctx, component, &pending).await;
    }

    match id {
        card::CANCEL => {
            state
                .pending
                .lock()
                .map_err(lock_err)?
                .remove(&component.message.id);
            component
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::UpdateMessage(
                        CreateInteractionResponseMessage::new()
                            .content("Cancelled — nothing was filed.")
                            .embeds(vec![])
                            .components(vec![]),
                    ),
                )
                .await
                .context("cancelling the draft")?;
            Ok(())
        }

        card::EDIT => component
            .create_response(
                &ctx.http,
                CreateInteractionResponse::Modal(card::title_modal(&pending.draft.title)),
            )
            .await
            .context("opening the title editor"),

        card::LABELS => {
            let ComponentInteractionDataKind::StringSelect { values } = &component.data.kind else {
                return Ok(());
            };
            let chosen = valid_labels(values, &pending.available_labels);
            let updated = mutate(&state, component.message.id, |p| p.draft.labels = chosen)?;
            refresh(ctx, component, &updated).await
        }

        card::CREATE => {
            // Filing takes longer than Discord's three-second reply window.
            component
                .create_response(&ctx.http, CreateInteractionResponse::Acknowledge)
                .await
                .context("acknowledging the confirmation")?;

            match file_issue(&state, &pending).await {
                Ok(issue) => {
                    tracing::info!(repo = %pending.repo, number = issue.number, "filed an issue");
                    state
                        .pending
                        .lock()
                        .map_err(lock_err)?
                        .remove(&component.message.id);
                    state
                        .filed
                        .lock()
                        .map_err(lock_err)?
                        .insert(pending.channel_id, issue.html_url.clone());
                    component
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content(format!("Filed: {}", issue.html_url))
                                .embeds(vec![])
                                .components(vec![]),
                        )
                        .await
                        .context("reporting the filed issue")?;
                }
                Err(e) => {
                    // Leave the card usable so they can just press it again.
                    tracing::warn!(error = ?e, "filing failed");
                    component
                        .edit_response(
                            &ctx.http,
                            EditInteractionResponse::new()
                                .content(format!("Could not file the issue: {e}"))
                                .embed(card::embed(&pending))
                                .components(card::components(&pending)),
                        )
                        .await
                        .context("reporting a failed filing")?;
                }
            }
            Ok(())
        }

        _ => Ok(()),
    }
}

/// The **Edit title** modal coming back.
async fn on_modal(state: Arc<BotState>, ctx: &Context, modal: &ModalInteraction) -> Result<()> {
    if modal.data.custom_id != card::TITLE_MODAL {
        return Ok(());
    }
    let Some(message) = &modal.message else {
        return Ok(());
    };

    let Some(pending) = lookup(&state, message.id)? else {
        return expired_modal(ctx, modal).await;
    };
    if modal.user.id != pending.requester {
        return Ok(());
    }

    let title = modal
        .data
        .components
        .iter()
        .flat_map(|row| row.components.iter())
        .find_map(|c| match c {
            ActionRowComponent::InputText(input) if input.custom_id == card::TITLE_INPUT => {
                input.value.clone()
            }
            _ => None,
        })
        .unwrap_or_default();
    let title = title.trim().to_owned();
    if title.is_empty() {
        return Ok(());
    }

    let updated = mutate(&state, message.id, |p| p.draft.title = title)?;
    modal
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .embed(card::embed(&updated))
                    .components(card::components(&updated)),
            ),
        )
        .await
        .context("applying the edited title")
}

async fn file_issue(state: &BotState, pending: &PendingDraft) -> Result<CreatedIssue> {
    let body = render_body(
        &pending.draft,
        &pending.transcript,
        &IssueContext {
            thread_url: &pending.thread_url,
            reporter: &pending.reporter,
        },
        state.cfg.limits.max_body_chars,
    );
    state
        .github
        .create_issue(&pending.repo, &pending.draft.title, &body, &pending.draft.labels)
        .await
}

/// Keep only labels the repo still has, deduped and capped.
///
/// The values come from options we built ourselves, but a label can be deleted
/// on GitHub while a card sits open, and Discord's `max_values` is a client
/// hint rather than something to rely on.
fn valid_labels(values: &[String], available: &[String]) -> Vec<String> {
    let mut chosen: Vec<String> = Vec::new();
    for value in values {
        if available.contains(value) && !chosen.contains(value) {
            chosen.push(value.clone());
        }
    }
    chosen.truncate(crate::llm::MAX_LABELS);
    chosen
}

fn lookup(state: &BotState, card_id: MessageId) -> Result<Option<PendingDraft>> {
    Ok(state.pending.lock().map_err(lock_err)?.get(&card_id).cloned())
}

/// Apply an edit to a stored draft and hand back the updated copy.
fn mutate(
    state: &BotState,
    card_id: MessageId,
    edit: impl FnOnce(&mut PendingDraft),
) -> Result<PendingDraft> {
    let mut pending = state.pending.lock().map_err(lock_err)?;
    let draft = pending
        .get_mut(&card_id)
        .ok_or_else(|| anyhow!("this draft is no longer open"))?;
    edit(draft);
    Ok(draft.clone())
}

async fn refresh(ctx: &Context, component: &ComponentInteraction, pending: &PendingDraft) -> Result<()> {
    component
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .embed(card::embed(pending))
                    .components(card::components(pending)),
            ),
        )
        .await
        .context("updating the card")
}

/// Drafts do not survive a restart, so say that rather than fail silently.
async fn expired(ctx: &Context, component: &ComponentInteraction) -> Result<()> {
    component
        .create_response(
            &ctx.http,
            ephemeral("This draft is no longer open — run `@issue` again to start over."),
        )
        .await
        .context("reporting an expired draft")
}

async fn expired_modal(ctx: &Context, modal: &ModalInteraction) -> Result<()> {
    modal
        .create_response(
            &ctx.http,
            ephemeral("This draft is no longer open — run `@issue` again to start over."),
        )
        .await
        .context("reporting an expired draft")
}

async fn refuse(ctx: &Context, component: &ComponentInteraction, pending: &PendingDraft) -> Result<()> {
    component
        .create_response(
            &ctx.http,
            ephemeral(&format!(
                "Only {} can confirm this draft. Run `@issue` yourself to make your own.",
                pending.reporter
            )),
        )
        .await
        .context("refusing an interaction")
}

fn ephemeral(content: &str) -> CreateInteractionResponse {
    CreateInteractionResponse::Message(
        CreateInteractionResponseMessage::new()
            .content(content)
            .ephemeral(true),
    )
}

fn lock_err<T>(_: T) -> anyhow::Error {
    anyhow!("internal lock poisoned")
}

/// The thread's starter message followed by every human reply, oldest first.
async fn collect_thread(
    ctx: &Context,
    thread: &GuildChannel,
    trigger_id: MessageId,
    max_messages: usize,
) -> Result<Vec<ThreadMessage>> {
    let mut out = Vec::new();

    // A thread and its starter message share an id. Forum posts answer on the
    // thread itself; threads started from a channel message answer on the parent.
    let starter_id = MessageId::new(thread.id.get());
    let starter = match thread.id.message(&ctx.http, starter_id).await {
        Ok(m) => Some(m),
        Err(_) => match thread.parent_id {
            Some(parent) => parent.message(&ctx.http, starter_id).await.ok(),
            None => None,
        },
    };
    if let Some(starter) = &starter {
        out.push(convert(starter));
    }

    let mut replies: Vec<Message> = Vec::new();
    let mut before: Option<MessageId> = None;
    while replies.len() < max_messages {
        let mut builder = GetMessages::new().limit(100);
        if let Some(before) = before {
            builder = builder.before(before);
        }
        let batch = thread
            .id
            .messages(&ctx.http, builder)
            .await
            .context("reading the thread")?;
        let Some(oldest) = batch.last() else { break };
        before = Some(oldest.id);
        let exhausted = batch.len() < 100;
        replies.extend(batch);
        if exhausted {
            break;
        }
    }

    // Discord hands these back newest-first.
    replies.reverse();
    let starter_id = starter.as_ref().map(|m| m.id);
    out.extend(
        replies
            .iter()
            .filter(|m| !m.author.bot && m.id != trigger_id && Some(m.id) != starter_id)
            .map(convert),
    );

    out.truncate(max_messages);
    Ok(out)
}

fn convert(m: &Message) -> ThreadMessage {
    ThreadMessage {
        author: display_name(&m.author),
        timestamp: m.timestamp.to_string(),
        content: m.content.clone(),
        assets: m
            .attachments
            .iter()
            .map(|a| Asset {
                name: a.filename.clone(),
                url: a.url.clone(),
                is_image: a.content_type.as_deref().is_some_and(|t| t.starts_with("image/")),
            })
            .collect(),
    }
}

fn display_name(user: &User) -> String {
    user.global_name.clone().unwrap_or_else(|| user.name.clone())
}

async fn reply(ctx: &Context, msg: &Message, text: &str) {
    // Discord rejects messages over 2000 characters.
    let text: String = crate::transcript::truncate_chars(text, 1_900).0;
    if let Err(e) = msg.reply(&ctx.http, text).await {
        tracing::warn!(error = %e, "could not reply in the thread");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::IssueDraft;
    use crate::transcript::{self, ThreadMessage};

    fn state() -> Arc<BotState> {
        Arc::new(BotState::new(
            Arc::new(crate::testutil::test_config("http://unused")),
            reqwest::Client::new(),
        ))
    }

    fn pending() -> PendingDraft {
        let script = transcript::build(
            &[ThreadMessage {
                author: "alice".into(),
                timestamp: "2026-09-24T10:00:00Z".into(),
                content: "the export button 500s".into(),
                assets: vec![],
            }],
            &crate::config::Limits::default(),
        );
        PendingDraft {
            channel_id: ChannelId::new(7),
            requester: UserId::new(99),
            repo: "octocat/hello".parse().unwrap(),
            draft: IssueDraft {
                title: "Export returns 500".into(),
                summary: "**What's happening**\nIt 500s.".into(),
                labels: vec!["bug".into()],
                from_model: true,
            },
            available_labels: vec!["bug".into(), "p2".into()],
            transcript: script,
            thread_url: "https://discord.com/channels/1/7".into(),
            reporter: "alice".into(),
        }
    }

    #[test]
    fn edits_apply_to_the_stored_draft() {
        let state = state();
        let card = MessageId::new(1);
        state.pending.lock().unwrap().insert(card, pending());

        let updated = mutate(&state, card, |p| p.draft.title = "Better title".into()).unwrap();
        assert_eq!(updated.title_for_test(), "Better title");
        // The edit must be persisted, not just returned.
        assert_eq!(
            lookup(&state, card).unwrap().unwrap().title_for_test(),
            "Better title"
        );
    }

    #[test]
    fn a_draft_that_is_gone_reads_as_gone() {
        let state = state();
        assert!(lookup(&state, MessageId::new(404)).unwrap().is_none());
        assert!(mutate(&state, MessageId::new(404), |_| {}).is_err());
    }

    #[test]
    fn filing_is_blocked_while_a_card_is_open_in_the_thread() {
        let state = state();
        state.pending.lock().unwrap().insert(MessageId::new(1), pending());
        let open = state
            .pending
            .lock()
            .unwrap()
            .values()
            .any(|p| p.channel_id == ChannelId::new(7));
        assert!(open, "a second @issue in the same thread must find the open card");
    }

    #[test]
    fn label_picks_are_checked_against_the_repo() {
        let available = vec![
            "bug".to_owned(),
            "p2".to_owned(),
            "docs".to_owned(),
            "ux".to_owned(),
        ];
        // A label deleted on GitHub while the card was open.
        assert_eq!(
            valid_labels(&["bug".into(), "deleted".into()], &available),
            vec!["bug"]
        );
        // Duplicates collapse.
        assert_eq!(
            valid_labels(&["bug".into(), "bug".into()], &available),
            vec!["bug"]
        );
        // More than we allow is trimmed even if Discord sent it.
        assert_eq!(
            valid_labels(
                &["bug".into(), "p2".into(), "docs".into(), "ux".into()],
                &available
            )
            .len(),
            crate::llm::MAX_LABELS
        );
    }
}
