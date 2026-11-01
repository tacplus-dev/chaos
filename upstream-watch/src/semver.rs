// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Semantic version parsing and ordering (semver.org 2.0.0 precedence), enough for release tags
//! such as `v2.8.7` or `v2.9.0-rc.1`. Build metadata (`+...`) is ignored for ordering.

use std::cmp::Ordering;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// Pre-release identifiers (`rc.1` gives `["rc", "1"]`); empty for a release.
    pub pre: Vec<String>,
}

impl Version {
    /// Parses `MAJOR.MINOR.PATCH[-PRE][+BUILD]`, with an optional leading `v`.
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        let s = s.strip_prefix('v').or_else(|| s.strip_prefix('V')).unwrap_or(s);
        let s = s.split('+').next()?;
        let (core, pre) = match s.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (s, None),
        };
        let mut parts = core.split('.');
        let mut num = || -> Option<u64> {
            let p = parts.next()?;
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            p.parse().ok()
        };
        let (major, minor, patch) = (num()?, num()?, num()?);
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some(p) => {
                let ids: Vec<String> = p.split('.').map(str::to_string).collect();
                if ids.iter().any(|i| i.is_empty()) {
                    return None;
                }
                ids
            }
            None => Vec::new(),
        };
        Some(Version { major, minor, patch, pre })
    }
}

fn cmp_identifier(a: &str, b: &str) -> Ordering {
    let num = |s: &str| -> Option<u64> {
        if s.bytes().all(|c| c.is_ascii_digit()) {
            s.parse().ok()
        } else {
            None
        }
    };
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.cmp(&y),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => a.cmp(b),
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (a, b) in self.pre.iter().zip(other.pre.iter()) {
                        let o = cmp_identifier(a, b);
                        if o != Ordering::Equal {
                            return o;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap_or_else(|| panic!("parse {s}"))
    }

    #[test]
    fn parses_tags() {
        assert_eq!(v("v2.8.7"), Version { major: 2, minor: 8, patch: 7, pre: vec![] });
        assert_eq!(v("2.8.7").pre, Vec::<String>::new());
        assert_eq!(v("v2.9.0-rc.1").pre, vec!["rc".to_string(), "1".to_string()]);
        assert_eq!(v("v1.2.3+build.5"), v("1.2.3"));
        for bad in ["", "v2.8", "v2.8.7.1", "v2.x.7", "latest", "v2.8.7-", "v2..7"] {
            assert!(Version::parse(bad).is_none(), "{bad} should not parse");
        }
    }

    #[test]
    fn orders_numerically_not_lexically() {
        assert!(v("v2.8.10") > v("v2.8.7"));
        assert!(v("v2.10.0") > v("v2.9.9"));
        assert!(v("v10.0.0") > v("v9.9.9"));
        assert!(v("v9.9.9") > v("v2.8.7"));
        assert!(v("v2.8.6") < v("v2.8.7"));
        assert_eq!(v("v2.8.7").cmp(&v("2.8.7")), Ordering::Equal);
    }

    #[test]
    fn pre_releases_order_below_the_release() {
        assert!(v("v2.9.0-rc.1") < v("v2.9.0"));
        assert!(v("v2.9.0-rc.1") > v("v2.8.7"));
        assert!(v("v2.9.0-alpha") < v("v2.9.0-alpha.1"));
        assert!(v("v2.9.0-alpha.1") < v("v2.9.0-beta"));
        assert!(v("v2.9.0-rc.2") < v("v2.9.0-rc.10"));
        assert!(v("v2.9.0-1") < v("v2.9.0-alpha"));
    }
}
