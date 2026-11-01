// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The nightly upstream watch (tacplus-dev/chaos#1): compares Chaos Mesh's releases newer than the
//! pin in `docs/chaos-mesh-survey.md` with the survey's section 6 table, writes one suggestion file
//! per new `### Added` changelog entry, and optionally records the compared releases in the survey.

pub mod changelog;
pub mod semver;
pub mod source;
pub mod suggestion;
pub mod survey;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use semver::Version;
use source::Source;

pub struct Options {
    pub survey: PathBuf,
    pub out: PathBuf,
    pub update_survey: bool,
    /// The run date, `YYYY-MM-DD`, used in suggestion file names.
    pub date: String,
}

/// Runs the watch and returns the paths it wrote (suggestions, then the survey if updated).
pub fn run(opts: &Options, source: &dyn Source) -> Result<Vec<PathBuf>, String> {
    let survey_text = read(&opts.survey)?;
    let survey = survey::parse(&survey_text)?;
    let pin = Version::parse(&survey.pin).ok_or_else(|| format!("survey: pin {:?} is not a version", survey.pin))?;

    let mut newer: Vec<(Version, source::Release)> = source
        .releases()?
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease)
        .filter_map(|r| Version::parse(&r.tag_name).map(|v| (v, r)))
        .filter(|(v, _)| *v > pin)
        .collect();
    newer.sort_by(|a, b| a.0.cmp(&b.0));

    let mut seen: BTreeSet<u32> = survey.reviewed_prs.clone();
    let mut rows = Vec::new();
    let mut written = Vec::new();
    for (version, release) in &newer {
        let tag = &release.tag_name;
        let changelog = source.changelog(tag)?;
        let section = changelog::section(&changelog, &format!("{}.{}.{}", version.major, version.minor, version.patch))
            .ok_or_else(|| format!("CHANGELOG.md at {tag} has no `## [{tag}]` section"))?;
        let date = release
            .published_at
            .as_deref()
            .and_then(|p| p.get(..10))
            .map(str::to_string)
            .or(section.date.clone())
            .unwrap_or_else(|| "unknown date".to_string());

        for entry in &section.added {
            if !entry.prs.is_empty() && entry.prs.iter().any(|n| seen.contains(n)) {
                continue;
            }
            seen.extend(entry.prs.iter().copied());
            std::fs::create_dir_all(&opts.out).map_err(|e| format!("{}: {e}", opts.out.display()))?;
            let path = unique_path(&opts.out, &suggestion::file_name(&opts.date, entry), &written);
            write(&path, &suggestion::render(entry, tag))?;
            written.push(path);
        }
        rows.push(survey::Row { tag: tag.clone(), date, added: section.added });
    }

    if opts.update_survey {
        if let Some(last) = rows.last() {
            let updated = survey::update(&survey_text, &last.tag, &last.date, &rows)?;
            write(&opts.survey, &updated)?;
            written.push(opts.survey.clone());
        }
    }
    Ok(written)
}

/// `name`, or `name` with `-2`, `-3`, ... before `.md` if this run already wrote it.
fn unique_path(dir: &Path, name: &str, written: &[PathBuf]) -> PathBuf {
    let mut path = dir.join(name);
    let stem = name.trim_end_matches(".md");
    let mut n = 2;
    while written.contains(&path) {
        path = dir.join(format!("{stem}-{n}.md"));
        n += 1;
    }
    path
}

fn read(p: &Path) -> Result<String, String> {
    std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn write(p: &Path, s: &str) -> Result<(), String> {
    std::fs::write(p, s).map_err(|e| format!("{}: {e}", p.display()))
}

/// Today's date in UTC, `YYYY-MM-DD`.
pub fn today_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    civil_from_days((secs / 86_400) as i64)
}

/// Days since 1970-01-01 to `YYYY-MM-DD` (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Whether `s` looks like `YYYY-MM-DD`.
pub fn is_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter().enumerate().all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(20_393), "2025-11-01");
        assert_eq!(civil_from_days(11_016), "2000-02-29");
        assert!(is_date("2026-11-02"));
        assert!(!is_date("2026-1-02"));
        assert!(is_date(&today_utc()));
    }
}
