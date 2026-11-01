// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The hard bounds on every fault parameter, compiled in (#1, design section 2). A value out of bound is
//! refused, never clamped. Config can only tighten a bound: a configured limit above the compiled-in one
//! is ignored.

/// Every fault that lasts lasts 1–300 s. There is no "until deleted".
pub const MAX_DURATION_S: u32 = 300;
pub const MAX_LATENCY_MS: u32 = 2000;
pub const MAX_LOSS_PCT: u32 = 50;
pub const MAX_PCT: u32 = 100;
pub const MAX_MEMORY_PCT_OF_LIMIT: u32 = 50;
/// A pod kill's grace period: 0 (at once) to 30 s.
pub const MAX_GRACE_PERIOD_S: u32 = 30;
/// The most faults live on one node at once.
pub const MAX_LIVE_FAULTS: u32 = 4;

/// The bounds in force: the compiled-in ones, tightened by config.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    pub max_duration_s: u32,
    pub max_latency_ms: u32,
    pub max_loss_pct: u32,
    pub max_memory_pct_of_limit: u32,
    pub max_live_faults: u32,
}

impl Default for Bounds {
    fn default() -> Self {
        Bounds {
            max_duration_s: MAX_DURATION_S,
            max_latency_ms: MAX_LATENCY_MS,
            max_loss_pct: MAX_LOSS_PCT,
            max_memory_pct_of_limit: MAX_MEMORY_PCT_OF_LIMIT,
            max_live_faults: MAX_LIVE_FAULTS,
        }
    }
}

/// `configured` when it is set (non-zero) and tighter than `compiled`, else `compiled`.
fn tighten(compiled: u32, configured: u32) -> u32 {
    if configured == 0 {
        compiled
    } else {
        configured.min(compiled)
    }
}

impl Bounds {
    /// The compiled-in bounds tightened by the configured ones (0 = not set).
    pub fn tightened(duration_s: u32, latency_ms: u32, loss_pct: u32, memory_pct: u32, live_faults: u32) -> Bounds {
        let d = Bounds::default();
        Bounds {
            max_duration_s: tighten(d.max_duration_s, duration_s),
            max_latency_ms: tighten(d.max_latency_ms, latency_ms),
            max_loss_pct: tighten(d.max_loss_pct, loss_pct),
            max_memory_pct_of_limit: tighten(d.max_memory_pct_of_limit, memory_pct),
            max_live_faults: tighten(d.max_live_faults, live_faults),
        }
    }

    pub fn check_duration(&self, duration_s: u32) -> Result<(), String> {
        within("duration_s", duration_s, 1, self.max_duration_s)
    }

    /// netem delay: latency 1–max, jitter ≤ latency, correlation ≤ 100 %.
    pub fn check_network_delay(&self, latency_ms: u32, jitter_ms: u32, correlation_pct: u32, duration_s: u32) -> Result<(), String> {
        within("latency_ms", latency_ms, 1, self.max_latency_ms)?;
        if jitter_ms > latency_ms {
            return Err(format!("jitter_ms {jitter_ms} is above latency_ms {latency_ms}"));
        }
        within("correlation_pct", correlation_pct, 0, MAX_PCT)?;
        self.check_duration(duration_s)
    }

    /// netem loss: 1–max %, correlation ≤ 100 %.
    pub fn check_packet_loss(&self, loss_pct: u32, correlation_pct: u32, duration_s: u32) -> Result<(), String> {
        within("loss_pct", loss_pct, 1, self.max_loss_pct)?;
        within("correlation_pct", correlation_pct, 0, MAX_PCT)?;
        self.check_duration(duration_s)
    }

    /// Stress in the container's cgroup. CPU workers ≤ the container's CPU limit rounded up (one without a
    /// limit gets one worker), load 1–100 %; memory ≤ max % of the container's memory limit, and none at
    /// all for a container with no memory limit. At least one of CPU or memory.
    pub fn check_stress(
        &self,
        cpu_workers: u32,
        cpu_load_pct: u32,
        memory_pct_of_limit: u32,
        duration_s: u32,
        cpu_limit_millis: Option<u64>,
        memory_limit_bytes: Option<u64>,
    ) -> Result<(), String> {
        if cpu_workers == 0 && memory_pct_of_limit == 0 {
            return Err("a stress fault needs cpu_workers or memory_pct_of_limit".into());
        }
        if cpu_workers > 0 {
            let max_workers = cpu_limit_millis.map_or(1, |m| m.div_ceil(1000).max(1)) as u32;
            within("cpu_workers", cpu_workers, 1, max_workers)?;
            within("cpu_load_pct", cpu_load_pct, 1, MAX_PCT)?;
        } else if cpu_load_pct != 0 {
            return Err("cpu_load_pct is set without cpu_workers".into());
        }
        if memory_pct_of_limit > 0 {
            if memory_limit_bytes.is_none() {
                return Err("the container has no memory limit, so a memory stress has no bound".into());
            }
            within("memory_pct_of_limit", memory_pct_of_limit, 1, self.max_memory_pct_of_limit)?;
        }
        self.check_duration(duration_s)
    }

    pub fn check_grace_period(&self, grace_period_s: u32) -> Result<(), String> {
        within("grace_period_s", grace_period_s, 0, MAX_GRACE_PERIOD_S)
    }
}

fn within(name: &str, value: u32, min: u32, max: u32) -> Result<(), String> {
    if value < min || value > max {
        Err(format!("{name} {value} is out of bound ({min}–{max})"))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_in_bound_passes() {
        assert_eq!(Bounds::default().check_network_delay(200, 50, 25, 60), Ok(()));
        assert_eq!(Bounds::default().check_network_delay(2000, 2000, 100, 300), Ok(()));
    }

    #[test]
    fn delay_out_of_bound_is_refused_never_clamped() {
        let b = Bounds::default();
        assert!(b.check_network_delay(0, 0, 0, 60).unwrap_err().contains("latency_ms 0"));
        assert!(b.check_network_delay(2001, 0, 0, 60).unwrap_err().contains("latency_ms 2001"));
        assert!(b.check_network_delay(100, 101, 0, 60).unwrap_err().contains("jitter_ms"));
        assert!(b.check_network_delay(100, 0, 101, 60).unwrap_err().contains("correlation_pct"));
        assert!(b.check_network_delay(100, 0, 0, 0).unwrap_err().contains("duration_s 0"));
        assert!(b.check_network_delay(100, 0, 0, 301).unwrap_err().contains("duration_s 301"));
    }

    #[test]
    fn loss_bounds() {
        let b = Bounds::default();
        assert_eq!(b.check_packet_loss(50, 0, 10), Ok(()));
        assert!(b.check_packet_loss(0, 0, 10).is_err());
        assert!(b.check_packet_loss(51, 0, 10).unwrap_err().contains("loss_pct 51"));
    }

    #[test]
    fn stress_bounds_follow_the_containers_limits() {
        let b = Bounds::default();
        let gib = Some(1 << 30);
        assert_eq!(b.check_stress(2, 80, 0, 30, Some(1500), None), Ok(()));
        assert!(b.check_stress(3, 80, 0, 30, Some(1500), None).unwrap_err().contains("cpu_workers 3"));
        assert_eq!(b.check_stress(1, 100, 0, 30, None, None), Ok(()));
        assert!(b.check_stress(2, 100, 0, 30, None, None).is_err());
        assert!(b.check_stress(1, 0, 0, 30, None, None).unwrap_err().contains("cpu_load_pct 0"));
        assert_eq!(b.check_stress(0, 0, 50, 30, None, gib), Ok(()));
        assert!(b.check_stress(0, 0, 51, 30, None, gib).unwrap_err().contains("memory_pct_of_limit 51"));
        assert!(b.check_stress(0, 0, 10, 30, None, None).unwrap_err().contains("no memory limit"));
        assert!(b.check_stress(0, 0, 0, 30, None, gib).is_err());
    }

    #[test]
    fn config_only_tightens() {
        let b = Bounds::tightened(60, 5000, 10, 0, 9);
        assert_eq!(b.max_duration_s, 60);
        assert_eq!(b.max_latency_ms, MAX_LATENCY_MS, "a looser configured bound is ignored");
        assert_eq!(b.max_loss_pct, 10);
        assert_eq!(b.max_memory_pct_of_limit, MAX_MEMORY_PCT_OF_LIMIT);
        assert_eq!(b.max_live_faults, MAX_LIVE_FAULTS);
        assert!(b.check_duration(61).is_err());
        assert!(b.check_packet_loss(11, 0, 10).is_err());
    }

    #[test]
    fn grace_period() {
        assert_eq!(Bounds::default().check_grace_period(0), Ok(()));
        assert!(Bounds::default().check_grace_period(31).is_err());
    }
}
