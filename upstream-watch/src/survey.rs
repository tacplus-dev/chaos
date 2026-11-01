// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Reading and updating `docs/chaos-mesh-survey.md`: the pin in the header's "Version surveyed"
//! line (`Chaos Mesh **v2.8.7** (released 2026-10-28; .../tag/v2.8.7)`) and the section 6 table
//! `| Release | `Added` entries | Where |`, whose second column holds the reviewed `(#N)` PRs.

use std::collections::BTreeSet;

use crate::changelog::Entry;

const PIN_MARKER: &str = "**Version surveyed:**";
const PIN_PREFIX: &str = "Chaos Mesh **";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Survey {
    /// The pinned release tag, e.g. `v2.8.7`.
    pub pin: String,
    /// Every PR number in the second column of the section 6 table.
    pub reviewed_prs: BTreeSet<u32>,
}

/// A row to add to the section 6 table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub tag: String,
    pub date: String,
    pub added: Vec<Entry>,
}

pub fn parse(text: &str) -> Result<Survey, String> {
    let line = pin_line(text)?;
    let pin = pin_tag(line).ok_or_else(|| format!("survey: no `{PIN_PREFIX}<tag>**` in the pin line: {line}"))?;
    let (rows, _) = table(text)?;
    let mut reviewed_prs = BTreeSet::new();
    for row in rows {
        let cells = split_row(row);
        if let Some(cell) = cells.get(1) {
            reviewed_prs.extend(prs_in(cell));
        }
    }
    Ok(Survey { pin: pin.to_string(), reviewed_prs })
}

/// Moves the pin to `new_tag` / `new_date` and appends `rows` to the section 6 table.
pub fn update(text: &str, new_tag: &str, new_date: &str, rows: &[Row]) -> Result<String, String> {
    let old_line = pin_line(text)?;
    let old_tag = pin_tag(old_line).ok_or("survey: no pin tag")?;
    let mut new_line = old_line.replace(old_tag, new_tag);
    if let Some(i) = new_line.find("released ") {
        let start = i + "released ".len();
        let end = new_line[start..].find([';', ')', ',']).map_or(new_line.len(), |j| start + j);
        new_line.replace_range(start..end, new_date);
    }

    let (_, insert_at) = table(text)?;
    let mut out = String::with_capacity(text.len() + 256);
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        if offset == insert_at && !rows.is_empty() {
            if !out.ends_with('\n') {
                out.push('\n');
            }
            for row in rows {
                out.push_str(&render_row(row));
                out.push('\n');
            }
        }
        if std::ptr::eq(body.as_ptr(), old_line.as_ptr()) {
            out.push_str(&new_line);
            out.push_str(&line[body.len()..]);
        } else {
            out.push_str(line);
        }
        offset += line.len();
    }
    if offset == insert_at && !rows.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        for row in rows {
            out.push_str(&render_row(row));
            out.push('\n');
        }
    }
    Ok(out)
}

/// `| v9.9.9 (2026-11-01) | Add x (#9999); Add y (#1, #2) | To survey |`
pub fn render_row(row: &Row) -> String {
    let entries = if row.added.is_empty() {
        "none".to_string()
    } else {
        row.added
            .iter()
            .map(|e| {
                let text = e.text.replace('|', "\\|");
                if e.prs.is_empty() {
                    text
                } else {
                    let prs: Vec<String> = e.prs.iter().map(|n| format!("#{n}")).collect();
                    format!("{text} ({})", prs.join(", "))
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    format!("| {} ({}) | {} | To survey |", row.tag, row.date, entries)
}

fn pin_line(text: &str) -> Result<&str, String> {
    text.lines()
        .find(|l| l.contains(PIN_MARKER))
        .ok_or_else(|| format!("survey: no `{PIN_MARKER}` line"))
}

fn pin_tag(line: &str) -> Option<&str> {
    let after = &line[line.find(PIN_PREFIX)? + PIN_PREFIX.len()..];
    let tag = &after[..after.find("**")?];
    (!tag.is_empty()).then_some(tag)
}

/// The data rows of the section 6 table, and the byte offset just after its last row.
fn table(text: &str) -> Result<(Vec<&str>, usize), String> {
    let mut offset = 0;
    let mut in_section = false;
    let mut seen_table = false;
    let mut header_lines = 0;
    let mut rows = Vec::new();
    let mut end = None;
    for line in text.split_inclusive('\n') {
        let body = line.trim_end_matches(['\n', '\r']);
        let t = body.trim();
        if t.starts_with("## ") {
            if in_section {
                break;
            }
            in_section = t["## ".len()..].trim_start().starts_with("6.");
        } else if in_section {
            if t.starts_with('|') {
                seen_table = true;
                header_lines += 1;
                if header_lines > 2 {
                    rows.push(t);
                }
                end = Some(offset + line.len());
            } else if seen_table {
                break;
            }
        }
        offset += line.len();
    }
    match end {
        Some(end) if header_lines >= 2 => Ok((rows, end)),
        _ => Err("survey: no table in section 6 (\"## 6. Upstream changes reviewed\")".to_string()),
    }
}

/// Splits a markdown table row into its cells, honouring `\|`.
fn split_row(row: &str) -> Vec<String> {
    let row = row.trim().trim_start_matches('|');
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek() == Some(&'|') => {
                cur.push('|');
                chars.next();
            }
            '|' => cells.push(std::mem::take(&mut cur).trim().to_string()),
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        cells.push(cur.trim().to_string());
    }
    cells
}

/// PR numbers written `#N` in a cell (as in `(#5162)` or `(#1, #2)`).
fn prs_in(cell: &str) -> Vec<u32> {
    let mut out = Vec::new();
    let b = cell.as_bytes();
    for (i, _) in cell.match_indices('#') {
        let boundary = i == 0 || matches!(b[i - 1], b'(' | b' ' | b',' | b'[');
        let digits: String = cell[i + 1..].chars().take_while(char::is_ascii_digit).collect();
        if boundary && !digits.is_empty() {
            if let Ok(n) = digits.parse() {
                out.push(n);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SURVEY: &str = "# S\n\n- **Version surveyed:** Chaos Mesh **v2.8.7** (released 2026-10-28; https://github.com/chaos-mesh/chaos-mesh/releases/tag/v2.8.7).\n  more\n\n\
## 6. Upstream changes reviewed\n\nText.\n\n| Release | `Added` entries | Where |\n|---|---|---|\n\
| v2.8.7 (2026-10-28) | `a` for b (#5162); c (#4790) | Section 2 (see #1) |\n";

    #[test]
    fn reads_pin_and_reviewed_prs() {
        let s = parse(SURVEY).unwrap();
        assert_eq!(s.pin, "v2.8.7");
        assert_eq!(s.reviewed_prs, BTreeSet::from([4790, 5162]), "the Where column's #1 is not a PR");
    }

    #[test]
    fn update_moves_pin_and_appends_rows() {
        let rows = [
            Row { tag: "v2.9.0".into(), date: "2026-11-10".into(), added: vec![] },
            Row {
                tag: "v2.9.1".into(),
                date: "2026-11-20".into(),
                added: vec![Entry { text: "Add a|b".into(), prs: vec![7, 8] }],
            },
        ];
        let out = update(SURVEY, "v2.9.1", "2026-11-20", &rows).unwrap();
        let expected = SURVEY
            .replace("v2.8.7** (released 2026-10-28; https://github.com/chaos-mesh/chaos-mesh/releases/tag/v2.8.7)",
                     "v2.9.1** (released 2026-11-20; https://github.com/chaos-mesh/chaos-mesh/releases/tag/v2.9.1)")
            + "| v2.9.0 (2026-11-10) | none | To survey |\n| v2.9.1 (2026-11-20) | Add a\\|b (#7, #8) | To survey |\n";
        assert_eq!(out, expected);
        let s = parse(&out).unwrap();
        assert_eq!(s.pin, "v2.9.1");
        assert_eq!(s.reviewed_prs, BTreeSet::from([7, 8, 4790, 5162]));
    }

    #[test]
    fn rows_go_right_after_the_table_when_text_follows() {
        let text = SURVEY.to_string() + "\nAfter.\n\n## 7. Next\n";
        let rows = [Row { tag: "v3.0.0".into(), date: "2027-01-01".into(), added: vec![] }];
        let out = update(&text, "v3.0.0", "2027-01-01", &rows).unwrap();
        assert!(out.contains("(#4790) | Section 2 (see #1) |\n| v3.0.0 (2027-01-01) | none | To survey |\n\nAfter."));
    }

    #[test]
    fn missing_pieces_are_errors() {
        assert!(parse("# no pin\n").is_err());
        assert!(parse("- **Version surveyed:** Chaos Mesh **v1.0.0**\n## 6. X\nno table\n").is_err());
    }
}
