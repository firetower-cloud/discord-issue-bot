//! The confirmation card: what the bot proposes before anything is filed.
//!
//! Nothing reaches GitHub until someone presses **Create issue**, so the
//! model's draft is a suggestion rather than a decision.

use serenity::all::*;

use crate::config::Repo;
use crate::llm::{IssueDraft, MAX_LABELS};
use crate::transcript::{Transcript, truncate_chars};

pub const CREATE: &str = "issue.create";
pub const EDIT: &str = "issue.edit";
pub const CANCEL: &str = "issue.cancel";
pub const LABELS: &str = "issue.labels";
pub const TITLE_MODAL: &str = "issue.title_modal";
pub const TITLE_INPUT: &str = "issue.title_input";

/// Discord's hard cap on options in a select menu.
const MAX_SELECT_OPTIONS: usize = 25;
/// Embed field values cap at 1024; leave room for the truncation marker.
const SUMMARY_PREVIEW: usize = 1000;

/// A draft waiting on someone to confirm it.
#[derive(Clone)]
pub struct PendingDraft {
    pub channel_id: ChannelId,
    /// Only this user may confirm, edit or cancel — it is their report.
    pub requester: UserId,
    pub repo: Repo,
    pub draft: IssueDraft,
    pub available_labels: Vec<String>,
    pub transcript: Transcript,
    pub thread_url: String,
    pub reporter: String,
}

impl PendingDraft {
    #[cfg(test)]
    pub fn title_for_test(&self) -> &str {
        &self.draft.title
    }
}

pub fn embed(pending: &PendingDraft) -> CreateEmbed {
    let labels = if pending.draft.labels.is_empty() {
        "_none_".to_owned()
    } else {
        pending
            .draft
            .labels
            .iter()
            .map(|l| format!("`{l}`"))
            .collect::<Vec<_>>()
            .join(" ")
    };

    let summary = if pending.draft.summary.is_empty() {
        "_No summary — the thread will be filed as-is._".to_owned()
    } else {
        truncate_chars(&pending.draft.summary, SUMMARY_PREVIEW).0
    };

    let mut footer = format!(
        "{} messages · requested by {}",
        pending.transcript.total_messages, pending.reporter
    );
    if pending.transcript.body_truncated {
        footer.push_str(" · thread trimmed to fit");
    }
    if !pending.draft.from_model {
        footer.push_str(" · drafted without the model");
    }

    CreateEmbed::new()
        .title("Draft issue")
        .description(format!("Filing into **{}** once you confirm.", pending.repo))
        .field("Title", truncate_chars(&pending.draft.title, 1000).0, false)
        .field("Labels", labels, false)
        .field("Summary", summary, false)
        .footer(CreateEmbedFooter::new(footer))
        .colour(Colour::new(0x5865F2))
}

pub fn components(pending: &PendingDraft) -> Vec<CreateActionRow> {
    let mut rows = Vec::new();

    let options = label_options(&pending.available_labels, &pending.draft.labels);
    if !options.is_empty() {
        let max = MAX_LABELS.min(options.len()) as u8;
        rows.push(CreateActionRow::SelectMenu(
            CreateSelectMenu::new(LABELS, CreateSelectMenuKind::String { options })
                .placeholder("Labels")
                .min_values(0)
                .max_values(max),
        ));
    }

    rows.push(CreateActionRow::Buttons(vec![
        CreateButton::new(CREATE)
            .label("Create issue")
            .style(ButtonStyle::Success),
        CreateButton::new(EDIT)
            .label("Edit title")
            .style(ButtonStyle::Secondary),
        CreateButton::new(CANCEL)
            .label("Cancel")
            .style(ButtonStyle::Danger),
    ]));

    rows
}

/// The modal behind **Edit title**: one prefilled line.
pub fn title_modal(current_title: &str) -> CreateModal {
    CreateModal::new(TITLE_MODAL, "Edit the issue title").components(vec![CreateActionRow::InputText(
        CreateInputText::new(InputTextStyle::Short, "Title", TITLE_INPUT)
            .value(truncate_chars(current_title, 200).0)
            .max_length(200)
            .required(true),
    )])
}

/// Chosen labels first and pre-ticked, then the rest, capped at Discord's 25.
///
/// The cap matters: a repo with more than 25 labels would otherwise be
/// rejected outright by Discord, so the model's picks have to survive it.
pub fn label_options(available: &[String], chosen: &[String]) -> Vec<CreateSelectMenuOption> {
    let mut ordered: Vec<&String> = chosen.iter().filter(|c| available.contains(c)).collect();
    ordered.extend(available.iter().filter(|a| !chosen.contains(a)));
    ordered.truncate(MAX_SELECT_OPTIONS);

    ordered
        .into_iter()
        .map(|label| {
            let option = CreateSelectMenuOption::new(truncate_chars(label, 100).0, label);
            if chosen.contains(label) {
                option.default_selection(true)
            } else {
                option
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("label-{i}")).collect()
    }

    #[test]
    fn chosen_labels_come_first_and_survive_the_25_cap() {
        let available = names(40);
        let chosen = vec!["label-30".to_owned(), "label-39".to_owned()];
        let options = label_options(&available, &chosen);

        assert_eq!(options.len(), MAX_SELECT_OPTIONS, "Discord rejects more than 25");
        let json = serde_json::to_value(&options).unwrap();
        assert_eq!(json[0]["value"], "label-30");
        assert_eq!(json[1]["value"], "label-39");
        assert_eq!(
            json[0]["default"], true,
            "the model's picks must come back ticked"
        );
        assert_eq!(json[2]["default"], serde_json::Value::Null);
    }

    #[test]
    fn a_repo_without_labels_gets_no_menu() {
        assert!(label_options(&[], &[]).is_empty());
    }

    #[test]
    fn a_stale_label_choice_is_not_offered() {
        let options = label_options(&["bug".to_owned()], &["deleted-label".to_owned()]);
        let json = serde_json::to_value(&options).unwrap();
        assert_eq!(json.as_array().unwrap().len(), 1);
        assert_eq!(json[0]["value"], "bug");
    }
}
