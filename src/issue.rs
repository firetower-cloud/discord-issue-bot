//! Assembling the final GitHub issue body.

use crate::llm::IssueDraft;
use crate::transcript::{BODY_RESERVE, Transcript, truncate_chars};

/// The model is asked for a short summary, but nothing stops it returning an
/// essay. Cap it here so it cannot crowd out the thread or the footer.
const SUMMARY_CAP: usize = BODY_RESERVE - 1_500;

pub struct IssueContext<'a> {
    pub thread_url: &'a str,
    pub reporter: &'a str,
}

/// Summary · rule · thread content · rule · thread URL.
pub fn render_body(
    draft: &IssueDraft,
    transcript: &Transcript,
    cx: &IssueContext,
    max_chars: usize,
) -> String {
    let mut out = String::new();

    if draft.summary.is_empty() {
        out.push_str("_No summary available — the thread is reproduced in full below._");
    } else {
        out.push_str(&truncate_chars(&draft.summary, SUMMARY_CAP).0);
    }

    out.push_str("\n\n---\n\n### Thread\n\n");
    out.push_str(&transcript.body_text);

    let mut notes: Vec<String> = Vec::new();
    if transcript.body_truncated {
        notes.push(format!(
            "This thread has {} messages and was too long to reproduce in full; some of it was omitted.",
            transcript.total_messages
        ));
    }
    if transcript.assets_truncated {
        notes.push("Some attachments were left out.".to_owned());
    }
    if !draft.from_model {
        notes.push("The summary step was unavailable, so this issue is the raw thread.".to_owned());
    }
    if !notes.is_empty() {
        out.push_str(&format!("\n\n> [!NOTE]\n> {}", notes.join(" ")));
    }

    out.push_str(&format!(
        "\n\n---\n\n[View the original Discord thread]({})\n\n<sub>Filed from Discord by {}.</sub>",
        cx.thread_url, cx.reporter
    ));

    truncate_chars(&out, max_chars).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::transcript::{self, ThreadMessage};

    fn transcript_of(contents: &[&str]) -> Transcript {
        let messages: Vec<ThreadMessage> = contents
            .iter()
            .map(|c| ThreadMessage {
                author: "alice".to_owned(),
                timestamp: "2026-09-24T10:00:00Z".to_owned(),
                content: (*c).to_owned(),
                assets: vec![],
            })
            .collect();
        transcript::build(&messages, &Limits::default())
    }

    #[test]
    fn body_has_the_three_sections_in_order() {
        let draft = IssueDraft {
            title: "Export returns 500".to_owned(),
            summary: "**What's happening**\nExport 500s.".to_owned(),
            labels: vec!["bug".to_owned()],
            from_model: true,
        };
        let t = transcript_of(&["export button 500s", "confirmed"]);
        let cx = IssueContext {
            thread_url: "https://discord.com/channels/1/2",
            reporter: "alice",
        };
        let body = render_body(&draft, &t, &cx, 60_000);

        let summary_at = body.find("Export 500s.").unwrap();
        let thread_at = body.find("export button 500s").unwrap();
        let url_at = body.find("https://discord.com/channels/1/2").unwrap();
        assert!(
            summary_at < thread_at && thread_at < url_at,
            "summary, thread, then URL"
        );
        assert_eq!(
            body.matches("\n---\n").count(),
            2,
            "one rule between each section"
        );
    }

    #[test]
    fn an_overlong_summary_cannot_crowd_out_the_thread_url() {
        let draft = IssueDraft {
            title: "t".to_owned(),
            summary: "padding. ".repeat(40_000),
            labels: vec![],
            from_model: true,
        };
        let t = transcript_of(&["export button 500s"]);
        let cx = IssueContext {
            thread_url: "https://discord.com/channels/1/2",
            reporter: "alice",
        };
        let body = render_body(&draft, &t, &cx, Limits::default().max_body_chars);

        assert!(body.chars().count() <= Limits::default().max_body_chars);
        assert!(
            body.contains("export button 500s"),
            "the thread must survive a long summary"
        );
        assert!(body.ends_with("</sub>"), "the footer must be intact");
        assert!(
            body.contains("https://discord.com/channels/1/2"),
            "the thread URL must survive"
        );
    }

    #[test]
    fn body_never_exceeds_the_cap() {
        let draft = IssueDraft {
            title: "t".to_owned(),
            summary: "s".repeat(5_000),
            labels: vec![],
            from_model: true,
        };
        let t = transcript_of(&["x"; 50]);
        let cx = IssueContext {
            thread_url: "https://d/1",
            reporter: "alice",
        };
        let body = render_body(&draft, &t, &cx, 1_000);
        assert!(body.chars().count() <= 1_000);
    }

    #[test]
    fn a_missing_summary_is_called_out() {
        let draft = crate::llm::fallback("export button 500s");
        let t = transcript_of(&["export button 500s"]);
        let cx = IssueContext {
            thread_url: "https://d/1",
            reporter: "alice",
        };
        let body = render_body(&draft, &t, &cx, 60_000);
        assert!(body.contains("No summary available"));
        assert!(body.contains("The summary step was unavailable"));
    }
}
