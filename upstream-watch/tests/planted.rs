// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! End to end, with no network: a planted v9.9.9 release in `tests/fixtures/planted/` against a copy
//! of the real survey. Its `### Added` has one new entry (#9999), one already reviewed (#5162) and its
//! `### Fixed` one more (#9998); the releases also hold a draft and a pre-release, both to skip.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_chaos-upstream-watch");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/planted");
const SURVEY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../docs/chaos-mesh-survey.md");
const EXPECTED_NAME: &str = "2026-11-02-chaos-mesh-add-podchaos-action-pod-freeze-to.md";

fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("planted-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs the binary; returns the printed paths.
fn watch(survey: &Path, out: &Path, update: bool) -> Vec<String> {
    let mut cmd = Command::new(BIN);
    cmd.arg("--survey").arg(survey).arg("--out").arg(out).args(["--date", "2026-11-02", "--fixtures", FIXTURES]);
    if update {
        cmd.arg("--update-survey");
    }
    let o = cmd.output().unwrap();
    assert!(o.status.success(), "watch failed: {}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout).unwrap().lines().map(str::to_string).collect()
}

fn files_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.map(|e| e.unwrap().file_name().into_string().unwrap()).collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

#[test]
fn writes_exactly_the_one_new_suggestion_byte_for_byte() {
    let dir = scratch("suggest");
    let survey = dir.join("chaos-mesh-survey.md");
    std::fs::copy(SURVEY, &survey).unwrap();
    let out = dir.join("out");

    let printed = watch(&survey, &out, false);

    let expected_path = out.join(EXPECTED_NAME);
    assert_eq!(printed, vec![expected_path.display().to_string()]);
    assert_eq!(files_in(&out), vec![EXPECTED_NAME.to_string()]);
    let got = std::fs::read(&expected_path).unwrap();
    let want = std::fs::read(Path::new(FIXTURES).join("expected").join(EXPECTED_NAME)).unwrap();
    assert_eq!(String::from_utf8_lossy(&got), String::from_utf8_lossy(&want));
    assert_eq!(got, want);
    // Without --update-survey the survey is untouched.
    assert_eq!(std::fs::read(&survey).unwrap(), std::fs::read(SURVEY).unwrap());
}

#[test]
fn update_survey_moves_the_pin_and_a_second_run_writes_nothing() {
    let dir = scratch("update");
    let survey = dir.join("chaos-mesh-survey.md");
    std::fs::copy(SURVEY, &survey).unwrap();
    let original = std::fs::read_to_string(SURVEY).unwrap();

    let out1 = dir.join("out1");
    let printed = watch(&survey, &out1, true);
    assert_eq!(
        printed,
        vec![out1.join(EXPECTED_NAME).display().to_string(), survey.display().to_string()]
    );

    // Expected: the original with its pin line replaced and one row after section 6's table (the
    // survey's last table), nothing else changed.
    let lines: Vec<&str> = original.lines().collect();
    let last_row = lines.iter().rposition(|l| l.starts_with('|')).unwrap();
    let mut want = String::new();
    for (i, line) in lines.iter().enumerate() {
        if line.contains("**Version surveyed:**") {
            want.push_str("- **Version surveyed:** Chaos Mesh **v9.9.9** (released 2026-11-01; https://github.com/chaos-mesh/chaos-mesh/releases/tag/v9.9.9).");
        } else {
            want.push_str(line);
        }
        want.push('\n');
        if i == last_row {
            want.push_str("| v9.9.9 (2026-11-01) | Add PodChaos action pod-freeze to stop a pod's processes (#9999); Support `serviceAccountName` for Workflow Task pods (#5162) | To survey |\n");
        }
    }
    assert!(original.ends_with('\n'));
    assert_eq!(std::fs::read_to_string(&survey).unwrap(), want);

    let out2 = dir.join("out2");
    assert_eq!(watch(&survey, &out2, true), Vec::<String>::new());
    assert_eq!(files_in(&out2), Vec::<String>::new());
    assert_eq!(std::fs::read_to_string(&survey).unwrap(), want, "the second run leaves the survey alone");
}
