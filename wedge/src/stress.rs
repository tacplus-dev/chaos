// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The stress process: this binary run as `chaos-wedge stress <cpu_workers> <cpu_load_pct>
//! <memory_bytes> <duration_s>` by the wedge itself (src/kernel.rs), never by a caller. It loads the CPU
//! with busy workers at the given duty cycle and holds the given memory, touched, then exits at its end on
//! its own, so the load ends even if the wedge is gone.

use std::time::{Duration, Instant};

/// Parses the four numbers and runs the stress. Returns the exit code.
pub fn run(args: &[String]) -> i32 {
    let nums: Vec<u64> = args.iter().filter_map(|a| a.parse().ok()).collect();
    let [workers, load, memory, duration] = nums[..] else {
        eprintln!("usage: chaos-wedge stress <cpu_workers> <cpu_load_pct> <memory_bytes> <duration_s>");
        return 2;
    };
    let (load, duration) = (load.min(100), duration.min(chaos_policy::bounds::MAX_DURATION_S as u64));
    let end = Instant::now() + Duration::from_secs(duration);
    // Held, and every page touched, so it is resident and counts against the cgroup's limit.
    let mut held = vec![0u8; memory as usize];
    for i in (0..held.len()).step_by(4096) {
        held[i] = 1;
    }
    let workers: Vec<_> = (0..workers)
        .map(|_| {
            std::thread::spawn(move || {
                let slice = Duration::from_millis(100);
                while Instant::now() < end {
                    let busy_until = Instant::now() + slice * load as u32 / 100;
                    while Instant::now() < busy_until {
                        std::hint::spin_loop();
                    }
                    std::thread::sleep(slice * (100 - load as u32) / 100);
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
    let now = Instant::now();
    if now < end {
        std::thread::sleep(end - now);
    }
    std::hint::black_box(&held);
    0
}
