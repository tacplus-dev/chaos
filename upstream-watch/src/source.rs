// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Where releases and changelogs come from: GitHub, or a fixtures directory (tests, no network).

use std::path::PathBuf;

use serde::Deserialize;

pub const REPO: &str = "chaos-mesh/chaos-mesh";

/// The fields used from GitHub's `GET /repos/{owner}/{repo}/releases`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
}

pub trait Source {
    fn releases(&self) -> Result<Vec<Release>, String>;
    fn changelog(&self, tag: &str) -> Result<String, String>;
}

/// Reads `releases.json` and `CHANGELOG-<tag>.md` from a directory.
pub struct Fixtures(pub PathBuf);

impl Source for Fixtures {
    fn releases(&self) -> Result<Vec<Release>, String> {
        let p = self.0.join("releases.json");
        let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
    }

    fn changelog(&self, tag: &str) -> Result<String, String> {
        let p = self.0.join(format!("CHANGELOG-{tag}.md"));
        std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))
    }
}

/// GitHub: the releases API (with `GH_TOKEN` or `GITHUB_TOKEN` if set) and raw `CHANGELOG.md`.
pub struct GitHub {
    client: reqwest::blocking::Client,
    token: Option<String>,
}

impl GitHub {
    pub fn new() -> Result<GitHub, String> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!("chaos-upstream-watch/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| format!("http client: {e}"))?;
        let token = ["GH_TOKEN", "GITHUB_TOKEN"]
            .iter()
            .filter_map(|k| std::env::var(k).ok())
            .find(|t| !t.trim().is_empty());
        Ok(GitHub { client, token })
    }

    fn get(&self, url: &str, api: bool) -> Result<String, String> {
        let mut req = self.client.get(url);
        if api {
            req = req
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28");
            if let Some(t) = &self.token {
                req = req.header("Authorization", format!("Bearer {t}"));
            }
        }
        let resp = req.send().map_err(|e| format!("GET {url}: {e}"))?;
        let status = resp.status();
        let body = resp.text().map_err(|e| format!("GET {url}: {e}"))?;
        if !status.is_success() {
            return Err(format!("GET {url}: HTTP {status}: {}", body.chars().take(300).collect::<String>()));
        }
        Ok(body)
    }
}

impl Source for GitHub {
    fn releases(&self) -> Result<Vec<Release>, String> {
        // The newest 100 releases are plenty for a nightly comparison with the pin.
        let url = format!("https://api.github.com/repos/{REPO}/releases?per_page=100");
        let body = self.get(&url, true)?;
        serde_json::from_str(&body).map_err(|e| format!("{url}: {e}"))
    }

    fn changelog(&self, tag: &str) -> Result<String, String> {
        self.get(&format!("https://raw.githubusercontent.com/{REPO}/{tag}/CHANGELOG.md"), false)
    }
}
