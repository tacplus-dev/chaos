// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! `chaos-upstream-watch --survey docs/chaos-mesh-survey.md --out <dir> [--update-survey]
//! [--date YYYY-MM-DD] [--fixtures <dir>]`: prints the paths it wrote, one per line.

use std::path::PathBuf;
use std::process::ExitCode;

use chaos_upstream_watch::source::{Fixtures, GitHub, Source};
use chaos_upstream_watch::{is_date, run, today_utc, Options};

const USAGE: &str = "usage: chaos-upstream-watch --survey <survey.md> --out <dir> [--update-survey] [--date YYYY-MM-DD] [--fixtures <dir>]";

fn main() -> ExitCode {
    match main_() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("chaos-upstream-watch: {e}");
            ExitCode::FAILURE
        }
    }
}

fn main_() -> Result<(), String> {
    let mut survey = None;
    let mut out = None;
    let mut update_survey = false;
    let mut date = None;
    let mut fixtures = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{a} needs a value\n{USAGE}"));
        match a.as_str() {
            "--survey" => survey = Some(PathBuf::from(value()?)),
            "--out" => out = Some(PathBuf::from(value()?)),
            "--update-survey" => update_survey = true,
            "--date" => date = Some(value()?),
            "--fixtures" => fixtures = Some(PathBuf::from(value()?)),
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(format!("unknown argument {a:?}\n{USAGE}")),
        }
    }
    let date = date.unwrap_or_else(today_utc);
    if !is_date(&date) {
        return Err(format!("--date {date:?} is not YYYY-MM-DD"));
    }
    let opts = Options {
        survey: survey.ok_or(format!("--survey is required\n{USAGE}"))?,
        out: out.ok_or(format!("--out is required\n{USAGE}"))?,
        update_survey,
        date,
    };
    let source: Box<dyn Source> = match fixtures {
        Some(dir) => Box::new(Fixtures(dir)),
        None => Box::new(GitHub::new()?),
    };
    for path in run(&opts, source.as_ref())? {
        println!("{}", path.display());
    }
    Ok(())
}
