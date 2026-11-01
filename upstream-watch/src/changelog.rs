// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Parser for Chaos Mesh's `CHANGELOG.md` (Keep a Changelog): `## [2.8.7] - 2026-10-28` release
//! headings, `### Added` / `### Changed` / ... subsections, and `- entry text [#5162](url)` items.
//! A PR reference may be `[#N](url)`, `(#N)` or a bare `#N`.

/// One changelog item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The item's text without its PR references, whitespace collapsed.
    pub text: String,
    /// The PR numbers it references, in order.
    pub prs: Vec<u32>,
}

/// One release's section of the changelog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Section {
    /// The date in the heading (`## [2.8.7] - 2026-10-28`), if any.
    pub date: Option<String>,
    /// The items of its `### Added` subsection ("Nothing" placeholders dropped).
    pub added: Vec<Entry>,
}

/// Returns the section for `version` (with or without a leading `v`), or `None` if the changelog
/// has no `## [version]` heading.
pub fn section(changelog: &str, version: &str) -> Option<Section> {
    let version = version.strip_prefix('v').unwrap_or(version);
    let mut lines = changelog.lines();
    let mut date = None;
    let mut found = false;
    for line in lines.by_ref() {
        if let Some((v, rest)) = release_heading(line) {
            if v.strip_prefix('v').unwrap_or(v) == version {
                date = parse_date(rest);
                found = true;
                break;
            }
        }
    }
    if !found {
        return None;
    }

    let mut added: Vec<String> = Vec::new();
    let mut in_added = false;
    for line in lines {
        let t = line.trim_start();
        if t.starts_with("## ") {
            break;
        }
        if let Some(sub) = t.strip_prefix("### ") {
            in_added = sub.trim().eq_ignore_ascii_case("added");
            continue;
        }
        if !in_added || t.trim().is_empty() {
            continue;
        }
        if let Some(item) = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            if line.len() - t.len() < 2 {
                added.push(item.to_string());
                continue;
            }
        }
        // A continuation (or nested) line belongs to the previous item.
        if let Some(last) = added.last_mut() {
            last.push(' ');
            last.push_str(t.trim());
        }
    }

    let added = added
        .iter()
        .map(|s| parse_entry(s))
        .filter(|e| {
            let t = e.text.trim_end_matches('.').trim();
            !(e.prs.is_empty() && (t.is_empty() || t.eq_ignore_ascii_case("nothing")))
        })
        .collect();
    Some(Section { date, added })
}

/// `## [2.8.7] - 2026-10-28` gives `("2.8.7", " - 2026-10-28")`.
fn release_heading(line: &str) -> Option<(&str, &str)> {
    let rest = line.trim().strip_prefix("## ")?.trim_start();
    let rest = rest.strip_prefix('[')?;
    let (v, rest) = rest.split_once(']')?;
    Some((v.trim(), rest))
}

fn parse_date(rest: &str) -> Option<String> {
    let d = rest.trim().trim_start_matches(['-', '–', ' ']).trim();
    let d: String = d.chars().take(10).collect();
    let b = d.as_bytes();
    let ok = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    ok.then_some(d)
}

/// Splits an item's text into its text and PR references.
pub fn parse_entry(item: &str) -> Entry {
    let mut text = item.trim().to_string();
    let mut prs = Vec::new();
    // Find each `#N` (preceded by start, whitespace, `[` or `(`), remove it with its wrapping.
    let mut search_from = 0;
    while let Some(off) = text[search_from..].find('#') {
        let hash = search_from + off;
        let digits_end = text[hash + 1..]
            .find(|c: char| !c.is_ascii_digit())
            .map_or(text.len(), |i| hash + 1 + i);
        let before = text[..hash].chars().next_back();
        let boundary_ok = matches!(before, None | Some('[') | Some('(')) || before.is_some_and(char::is_whitespace);
        let after = text[digits_end..].chars().next();
        let end_ok = after.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if digits_end == hash + 1 || !boundary_ok || !end_ok {
            search_from = hash + 1;
            continue;
        }
        let Ok(n) = text[hash + 1..digits_end].parse::<u32>() else {
            search_from = digits_end;
            continue;
        };
        let (start, end) = match before {
            Some('[') if text[digits_end..].starts_with("](") => {
                match text[digits_end + 2..].find(')') {
                    Some(i) => (hash - 1, digits_end + 2 + i + 1),
                    None => (hash - 1, text.len()),
                }
            }
            Some('[') if text[digits_end..].starts_with(']') => (hash - 1, digits_end + 1),
            Some('(') if text[digits_end..].starts_with(')') => (hash - 1, digits_end + 1),
            _ => (hash, digits_end),
        };
        prs.push(n);
        text.replace_range(start..end, " ");
        search_from = start;
    }
    let mut text = collapse(&text);
    if !prs.is_empty() {
        // Drop the empty wrapping left by a removed reference list, e.g. "text (#1, #2)".
        loop {
            let t = collapse(
                &text
                    .replace("( ", "(")
                    .replace(" )", ")")
                    .replace("(,", "(")
                    .replace(",)", ")")
                    .replace("()", "")
                    .replace("[]", ""),
            );
            if t == text {
                break;
            }
            text = t;
        }
        text = text.trim_end_matches([',', ' ']).to_string();
    }
    Entry { text, prs }
}

fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "# Chaos Mesh Changelog\n\n## [Unreleased]\n\n### Added\n\n- Nothing\n\n### Fixed\n\n- Nothing\n\n\
## [2.8.7] - 2026-10-28\n\n### Added\n\n\
- Support `serviceAccountName` for Workflow Task pods [#5162](https://github.com/chaos-mesh/chaos-mesh/pull/5162)\n\
- Support `nodeSelector` and `tolerations` for workflow task pods [#4790](https://github.com/chaos-mesh/chaos-mesh/pull/4790)\n\n\
### Changed\n\n\
- Upgrade toda to v0.3.0 to remove empty backing directories after IOChaos recovery [#5138](https://github.com/chaos-mesh/chaos-mesh/pull/5138)\n\n\
### Fixed\n\n- Fix JVMChaos preparation failures [#5161](https://github.com/chaos-mesh/chaos-mesh/pull/5161)\n\n\
## [2.8.6] - 2026-09-01\n\n### Added\n\n- Older thing [#4000](https://github.com/chaos-mesh/chaos-mesh/pull/4000)\n";

    #[test]
    fn reads_only_the_added_items_of_the_requested_release() {
        let s = section(REAL, "v2.8.7").unwrap();
        assert_eq!(s.date.as_deref(), Some("2026-10-28"));
        assert_eq!(
            s.added,
            vec![
                Entry { text: "Support `serviceAccountName` for Workflow Task pods".into(), prs: vec![5162] },
                Entry { text: "Support `nodeSelector` and `tolerations` for workflow task pods".into(), prs: vec![4790] },
            ]
        );
        assert_eq!(section(REAL, "2.8.6").unwrap().added[0].prs, vec![4000]);
        assert!(section(REAL, "Unreleased").unwrap().added.is_empty(), "Nothing placeholders are dropped");
        assert!(section(REAL, "2.8.5").is_none());
    }

    #[test]
    fn release_without_added_subsection_has_no_entries() {
        let c = "## [3.0.0] - 2027-01-01\n\n### Fixed\n\n- Fix x [#1](u)\n";
        assert_eq!(section(c, "v3.0.0").unwrap(), Section { date: Some("2027-01-01".into()), added: vec![] });
    }

    #[test]
    fn continuation_lines_join_the_item() {
        let c = "## [3.0.0]\n### Added\n- Add a long\n  feature description #42\n- Second (#43)\n";
        let s = section(c, "3.0.0").unwrap();
        assert_eq!(s.date, None);
        assert_eq!(s.added[0], Entry { text: "Add a long feature description".into(), prs: vec![42] });
        assert_eq!(s.added[1], Entry { text: "Second".into(), prs: vec![43] });
    }

    #[test]
    fn pr_reference_forms() {
        let link = parse_entry("Add x [#9999](https://github.com/chaos-mesh/chaos-mesh/pull/9999)");
        assert_eq!(link, Entry { text: "Add x".into(), prs: vec![9999] });
        assert_eq!(parse_entry("Add x #12"), Entry { text: "Add x".into(), prs: vec![12] });
        assert_eq!(parse_entry("Add x (#12)"), Entry { text: "Add x".into(), prs: vec![12] });
        assert_eq!(parse_entry("Add x [#12]"), Entry { text: "Add x".into(), prs: vec![12] });
        assert_eq!(
            parse_entry("Add x [#1](u1) [#2](u2)"),
            Entry { text: "Add x".into(), prs: vec![1, 2] }
        );
        assert_eq!(parse_entry("Add x (#1, #2)"), Entry { text: "Add x".into(), prs: vec![1, 2] });
        // Not PR references: no digits, glued to a word, or an anchor.
        assert_eq!(parse_entry("Use C# and a#1 and #tag"), Entry { text: "Use C# and a#1 and #tag".into(), prs: vec![] });
        assert_eq!(parse_entry("Keep (parens) #5"), Entry { text: "Keep (parens)".into(), prs: vec![5] });
    }
}
