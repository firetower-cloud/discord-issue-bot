# discord-issue-bot

Turn a Discord thread into a well-formed GitHub issue by mentioning the bot in it.

```
@issue
```

The bot reads the thread's starter message, every reply, and the attachments on
all of them, asks an LLM for a title, labels and a summary, and posts it back as
a card to confirm:

```
┌─ Draft issue ─────────────────────────────────┐
│ Filing into octocat/hello once you confirm.   │
│                                               │
│ Title    Export button returns 500 on large…  │
│ Labels   `bug` `p2`                           │
│ Summary  **What's happening** …               │
│                                               │
│ 12 messages · requested by alice              │
└───────────────────────────────────────────────┘
  [ Labels ▾ ]
  [ Create issue ]  [ Edit title ]  [ Cancel ]
```

**Nothing reaches GitHub until someone presses Create issue.** The label menu
comes pre-ticked with the model's picks, **Edit title** opens a modal with the
title prefilled, and **Cancel** throws the draft away. Only the person who ran
`@issue` can press the buttons.

Once confirmed, the issue body is:

```
<summary>

---

### Thread

<the thread, verbatim>

---

[View the original Discord thread](https://discord.com/channels/…)
```

It replies in the thread with the issue link.

## How it decides what to write

| Field | Comes from |
| --- | --- |
| Title | The model. One specific line, clamped to 200 characters. |
| Labels | The model, but **only** labels that already exist on the repo — it is shown the repo's real label list and anything it invents is dropped. Up to 3. |
| Summary | The model: *what's happening*, *expected*, *context*. It is told to state only what the thread says. |
| Body | Built by the bot, not the model, from the template above. |

The title and labels are yours to change before filing; the summary is the
model's, and is quicker to fix on GitHub afterwards than in a Discord modal.

If OpenAI is unreachable, out of quota, or refuses, you still get a card — with
the first line of the thread as the title, no labels, and a note saying the
summary step was skipped. A broken API key should not lose someone's bug report,
and the title is editable anyway.

## Guards

A thread is untrusted input from anyone who can type in your server, so:

- **Size.** At most 200 messages, 2 000 characters each, 24 000 characters total
  to the model, 60 000 to the issue body. A thread over budget keeps its start
  and its end (where the report and the conclusion live) and drops the middle,
  with a marker saying how many messages went. Every limit is an env var.
- **Prompt injection.** The thread is wrapped in delimiters carrying a random
  per-request nonce, and the system prompt says everything inside them is data,
  never instructions. Content cannot forge the delimiter — an echo of the nonce
  is redacted before it reaches the model.
- **Hidden text.** Zero-width characters, bidi overrides and control codes are
  stripped, so nothing can read one way to a human and another to the model.
- **GitHub side effects.** `@mentions` and `#123` in thread content are defused
  before they go in the issue body, so quoting a thread cannot ping GitHub users
  or auto-close issues with a stray "fixes #12".
- **Cost and spam.** One request per user per 20 seconds, one run per thread at a
  time, one open draft per thread, and a thread that already has an issue gets
  the existing link back instead of a duplicate.
- **Who can file.** Only the person who ran `@issue` can confirm their draft.
  Optionally set `ALLOWED_ROLE_IDS` to restrict who can start one at all.
- **Nothing is filed unattended.** Every issue passes through a human pressing a
  button, so a bad draft costs a click rather than a GitHub notification.

## Setup

### 1. Discord

1. [Discord Developer Portal](https://discord.com/developers/applications) →
   **New Application** → **Bot**.
2. Name the bot `issue`, so typing `@issue` autocompletes to it.
3. Under **Privileged Gateway Intents**, enable **MESSAGE CONTENT INTENT**.
   Without it the bot connects happily and reads every message as empty.
4. Copy the token into `DISCORD_TOKEN`.
5. **OAuth2 → URL Generator**: scope `bot`, permissions *View Channels*,
   *Read Message History*, *Send Messages*, *Send Messages in Threads*,
   *Embed Links*, *Add Reactions*. Open the generated URL to invite it.
   *Embed Links* is what lets the bot post the draft card — without it every
   `@issue` fails with `50013 Missing Permissions`. Buttons and modals need no
   extra scope; `applications.commands` is only for slash commands.

   Or use this directly, with your application ID:

   ```
   https://discord.com/oauth2/authorize?client_id=<APPLICATION_ID>&permissions=274877992000&scope=bot
   ```

### 2. GitHub

A fine-grained PAT with **Issues: read & write** on the target repo (a classic
PAT with `repo` also works) in `GITHUB_TOKEN`, and `owner/repo` in
`GITHUB_REPO`.

### 3. OpenAI

An API key in `OPENAI_API_KEY`. The default model is `gpt-4o-mini`; override
with `OPENAI_MODEL`. `OPENAI_BASE_URL` points at anything OpenAI-compatible.

See [`.env.example`](.env.example) for every variable.

## Run it

```bash
cp .env.example .env   # fill it in
set -a; source .env; set +a
cargo run
```

Or:

```bash
docker build -t discord-issue-bot .
docker run --rm --env-file .env -p 8080:8080 discord-issue-bot
```

`GET /healthz` returns `ok`.

## Deploy to Cloud Run

This is a gateway bot: it holds a persistent WebSocket to Discord rather than
serving requests. Cloud Run will run it, but three settings are not optional.

```bash
gcloud run deploy discord-issue-bot \
  --source . \
  --region europe-west1 \
  --min-instances 1 \
  --max-instances 1 \
  --no-cpu-throttling \
  --no-allow-unauthenticated \
  --set-env-vars GITHUB_REPO=owner/repo \
  --set-secrets DISCORD_TOKEN=discord-token:latest,OPENAI_API_KEY=openai-key:latest,GITHUB_TOKEN=github-token:latest
```

- `--max-instances 1` — **the important one.** Two instances mean two gateway
  connections, so every `@issue` files the issue twice.
- `--min-instances 1` — at zero instances there is no WebSocket, so the bot is
  simply offline. This is a billed, always-on instance.
- `--no-cpu-throttling` — otherwise CPU is withdrawn between requests and the
  connection dies.
- `--no-allow-unauthenticated` — nothing needs to reach it from outside;
  Cloud Run's own startup probe still works.

Store the three secrets in Secret Manager rather than `--set-env-vars`.

## Known limitations

- **Discord attachment URLs expire.** Discord now signs CDN links and they stop
  working roughly 24 hours later, so images embedded in an issue will eventually
  break. The issue keeps the filename and the thread link. Re-hosting the assets
  would fix this properly and is not implemented.
- **Drafts and dedupe are in-memory.** A restart orphans any card still waiting
  on a button — pressing it then says the draft is no longer open, and you run
  `@issue` again. Likewise a restart forgets which threads were already filed,
  so a second `@issue` there would open a second issue.
- **Label menus cap at 25 options**, Discord's limit. On a repo with more labels
  than that, the model's picks are listed first and the rest fill the remaining
  slots; a label outside the 25 has to be added on GitHub.
- **Single shard.** Fine well past the 2 500-guild mark Discord starts requiring
  shards at, but not beyond.
- Messages from bots and webhooks are skipped when reading a thread.
- **A public bot with `GITHUB_REPO` set files from any server that invites it.**
  If you tick *Public Bot*, set only `GITHUB_REPO_MAP` and leave `GITHUB_REPO`
  unset — unmapped servers then get "no GitHub repo is configured" instead of
  filing into your repo.
- Only attachments are carried over, not link previews or embeds.

## Development

```bash
cargo test
cargo clippy --all-targets -- -D warnings
```

The guard rails are the part worth testing; `src/transcript.rs` holds them and
its tests cover flooding, oversized messages, hidden characters, forged
delimiters and GitHub reference defusing.
