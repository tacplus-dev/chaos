// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Suggestion files, in the format of tacplus-dev/looper#50: the four fields `Suggestion`, `Type`,
//! `Summary`, `Why` in that order, then the sections.

use crate::changelog::Entry;

/// At most this many words in a file name's slug.
pub const SLUG_WORDS: usize = 6;

/// The slug rule: lowercase the entry text, split it on every character that is not an ASCII
/// letter or digit, and join the first [`SLUG_WORDS`] non-empty words with `-`.
/// "Add PodChaos action pod-freeze to stop..." gives `add-podchaos-action-pod-freeze-to`.
pub fn slug(text: &str) -> String {
    let words: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .take(SLUG_WORDS)
        .map(str::to_ascii_lowercase)
        .collect();
    if words.is_empty() {
        "entry".to_string()
    } else {
        words.join("-")
    }
}

/// `<date>-chaos-mesh-<slug>.md`
pub fn file_name(date: &str, entry: &Entry) -> String {
    format!("{date}-chaos-mesh-{}.md", slug(&entry.text))
}

pub fn render(entry: &Entry, tag: &str) -> String {
    let text = &entry.text;
    let mut changes = String::new();
    if entry.prs.is_empty() {
        changes.push_str(&format!("- Change: https://github.com/chaos-mesh/chaos-mesh/blob/{tag}/CHANGELOG.md\n"));
    }
    for n in &entry.prs {
        changes.push_str(&format!("- Change: https://github.com/chaos-mesh/chaos-mesh/pull/{n}\n"));
    }
    format!(
        "Suggestion: Chaos Mesh {text} ({tag})\n\
Type: Resiliency\n\
Summary: {text}\n\
Why: Chaos Mesh added it in {tag}; tacplus's chaos survey (tacplus-dev/chaos docs/chaos-mesh-survey.md) has no decision for it yet.\n\
\n\
## Evidence\n\
- Release: https://github.com/chaos-mesh/chaos-mesh/releases/tag/{tag}\n\
{changes}\
\n\
## Cost\n\
Unknown until surveyed: add it to docs/chaos-mesh-survey.md with an adopt now / adopt later / not doing decision.\n\
\n\
## Touches\n\
- tacplus-dev/chaos#1 (the chaos system's design) and docs/chaos-mesh-survey.md, section 6.\n\
\n\
## Reuse considered\n\
tacplus's chaos system is informed by Chaos Mesh, not built on it (tacplus-dev/chaos#1): the question is whether to build our own equivalent.\n\
\n\
## Status\n\
open\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_rule() {
        assert_eq!(slug("Add PodChaos action pod-freeze to stop a pod's processes"), "add-podchaos-action-pod-freeze-to");
        assert_eq!(slug("Support `serviceAccountName` for Workflow Task pods"), "support-serviceaccountname-for-workflow-task-pods");
        assert_eq!(slug("Short"), "short");
        assert_eq!(slug("--- !!"), "entry");
    }

    #[test]
    fn render_starts_with_the_four_fields() {
        let e = Entry { text: "Add x".into(), prs: vec![1] };
        let s = render(&e, "v3.0.0");
        let first: Vec<&str> = s.lines().take(4).map(|l| l.split(':').next().unwrap()).collect();
        assert_eq!(first, ["Suggestion", "Type", "Summary", "Why"]);
        assert!(s.starts_with("Suggestion: Chaos Mesh Add x (v3.0.0)\n"));
        assert!(s.contains("- Change: https://github.com/chaos-mesh/chaos-mesh/pull/1\n\n## Cost"));
    }
}
