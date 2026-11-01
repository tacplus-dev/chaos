// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The effects themselves, on this node's kernel. Every one is built here from numbers and from what the
//! wedge resolved itself: the caller's request reaches no command line.
//!
//! - netem runs `tc` (iproute2, in the image) with an argument list built here, in the pod's network
//!   namespace (`setns` into `/proc/<pid>/ns/net` in the child before exec), on the interface `eth0`,
//!   with the wedge's own qdisc handle, so a revert removes only what a wedge added.
//! - stress runs this binary's own `stress` mode (src/stress.rs) as a child that joins the target
//!   container's cgroup before it execs, so the load counts against its limits; it stops itself at its end.

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;

/// The interface a pod's traffic goes through, in its own network namespace.
pub const POD_INTERFACE: &str = "eth0";
/// The handle of the root qdisc a wedge adds; a revert deletes this handle and nothing else.
pub const QDISC_HANDLE: &str = "7a3c:";

/// netem's parameters, all numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Netem {
    Delay { latency_ms: u32, jitter_ms: u32, correlation_pct: u32 },
    Loss { loss_pct: u32, correlation_pct: u32 },
}

impl Netem {
    /// The `tc qdisc add` arguments after `netem`.
    pub fn args(&self) -> Vec<String> {
        match *self {
            Netem::Delay { latency_ms, jitter_ms, correlation_pct } => {
                let mut a = vec!["delay".into(), format!("{latency_ms}ms")];
                if jitter_ms > 0 {
                    a.push(format!("{jitter_ms}ms"));
                    if correlation_pct > 0 {
                        a.push(format!("{correlation_pct}%"));
                    }
                }
                a
            }
            Netem::Loss { loss_pct, correlation_pct } => {
                let mut a = vec!["loss".into(), format!("{loss_pct}%")];
                if correlation_pct > 0 {
                    a.push(format!("{correlation_pct}%"));
                }
                a
            }
        }
    }
}

/// The stress process's parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StressSpec {
    pub cpu_workers: u32,
    pub cpu_load_pct: u32,
    pub memory_bytes: u64,
    pub duration_s: u32,
}

/// A started stress process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StressProcess {
    pub pid: u32,
    pub start_ticks: u64,
}

/// The node's kernel, as the wedge acts on it. `HostKernel` in production; the service tests use a
/// recording one.
#[async_trait]
pub trait Kernel: Send + Sync + 'static {
    /// The inode of `/proc/<pid>/ns/net`: which network namespace a fault is applied in.
    fn netns_inode(&self, pid: u32) -> Result<u64, String>;
    async fn add_netem(&self, pid: u32, netem: Netem) -> Result<(), String>;
    /// Removes the wedge's qdisc. Ok(false) when there was nothing to remove (it, or the netns, is gone).
    async fn remove_netem(&self, pid: u32, netns_inode: u64) -> Result<bool, String>;
    async fn start_stress(&self, cgroup_dir: &Path, spec: StressSpec) -> Result<StressProcess, String>;
    /// Stops the stress process. Ok(false) when it had already ended.
    async fn stop_stress(&self, process: StressProcess) -> Result<bool, String>;
}

/// The real kernel: `tc` in the pod's netns, a stress child in its cgroup.
pub struct HostKernel {
    /// Where the host's /proc is (hostPID gives the wedge the node's pids).
    pub proc_root: PathBuf,
    /// The stress mode's executable: this binary.
    pub self_exe: PathBuf,
    /// The interface netem is put on.
    pub interface: String,
}

impl Default for HostKernel {
    fn default() -> Self {
        HostKernel {
            proc_root: PathBuf::from("/proc"),
            self_exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("/proc/self/exe")),
            interface: POD_INTERFACE.into(),
        }
    }
}

impl HostKernel {
    fn netns_path(&self, pid: u32) -> PathBuf {
        self.proc_root.join(pid.to_string()).join("ns/net")
    }

    /// Runs `tc <args>` in the network namespace of `pid`.
    fn tc_in_netns(&self, pid: u32, args: &[String]) -> Result<std::process::Output, String> {
        let ns = fs::File::open(self.netns_path(pid)).map_err(|e| format!("open netns of pid {pid}: {e}"))?;
        let fd = ns.as_raw_fd();
        let mut cmd = Command::new("tc");
        cmd.args(args);
        // SAFETY: setns is async-signal-safe; it runs in the forked child before exec and touches only
        // the fd opened above, which stays open (and is inherited) until the child execs.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setns(fd, libc::CLONE_NEWNET) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let out = cmd.output().map_err(|e| format!("run tc: {e}"))?;
        drop(ns);
        Ok(out)
    }
}

/// Field 22 of /proc/<pid>/stat, the process's start time in clock ticks.
pub fn process_start_ticks(proc_root: &Path, pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    // The command name (field 2) is in parentheses and may hold spaces: count from after it.
    let rest = &stat[stat.rfind(')')? + 2..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

#[async_trait]
impl Kernel for HostKernel {
    fn netns_inode(&self, pid: u32) -> Result<u64, String> {
        fs::metadata(self.netns_path(pid)).map(|m| m.ino()).map_err(|e| format!("netns of pid {pid}: {e}"))
    }

    async fn add_netem(&self, pid: u32, netem: Netem) -> Result<(), String> {
        let mut args: Vec<String> = ["qdisc", "add", "dev", &self.interface, "root", "handle", QDISC_HANDLE, "netem"].map(String::from).to_vec();
        args.extend(netem.args());
        let out = self.tc_in_netns(pid, &args)?;
        if !out.status.success() {
            return Err(format!("tc {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(())
    }

    async fn remove_netem(&self, pid: u32, netns_inode: u64) -> Result<bool, String> {
        match self.netns_inode(pid) {
            Ok(inode) if inode == netns_inode => {}
            // The pod (its sandbox process) is gone, and its netns with the qdisc in it.
            _ => return Ok(false),
        }
        let args: Vec<String> = ["qdisc", "del", "dev", &self.interface, "root", "handle", QDISC_HANDLE].map(String::from).to_vec();
        let out = self.tc_in_netns(pid, &args)?;
        if out.status.success() {
            return Ok(true);
        }
        let err = String::from_utf8_lossy(&out.stderr);
        // Gone already (reverted before, or the interface went with the pod): success.
        if err.contains("No such file") || err.contains("Invalid handle") || err.contains("Cannot find device") || err.contains("Cannot delete qdisc with handle of zero") {
            return Ok(false);
        }
        Err(format!("tc {}: {}", args.join(" "), err.trim()))
    }

    async fn start_stress(&self, cgroup_dir: &Path, spec: StressSpec) -> Result<StressProcess, String> {
        // The child joins the target's cgroup itself, before it execs (writing "0" to cgroup.procs moves
        // the writer), so everything it allocates is charged to the target, never to the wedge.
        let procs = fs::OpenOptions::new()
            .write(true)
            .open(cgroup_dir.join("cgroup.procs"))
            .map_err(|e| format!("open {}/cgroup.procs: {e}", cgroup_dir.display()))?;
        let fd = procs.as_raw_fd();
        let mut cmd = Command::new(&self.self_exe);
        cmd.args([
            "stress".to_string(),
            spec.cpu_workers.to_string(),
            spec.cpu_load_pct.to_string(),
            spec.memory_bytes.to_string(),
            spec.duration_s.to_string(),
        ]);
        // SAFETY: write(2) is async-signal-safe; it runs in the forked child before exec, on the fd opened
        // above, which stays open until the spawn returns.
        unsafe {
            cmd.pre_exec(move || {
                if libc::write(fd, b"0".as_ptr().cast(), 1) != 1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().map_err(|e| format!("start stress in {}: {e}", cgroup_dir.display()))?;
        drop(procs);
        let pid = child.id();
        // Reap it whenever it ends, so its pid is not left a zombie.
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
        });
        let start_ticks = process_start_ticks(&self.proc_root, pid).ok_or("stress process vanished")?;
        Ok(StressProcess { pid, start_ticks })
    }

    async fn stop_stress(&self, process: StressProcess) -> Result<bool, String> {
        if process_start_ticks(&self.proc_root, process.pid) != Some(process.start_ticks) {
            return Ok(false);
        }
        // SAFETY: plain kill(2); the start time check above makes it the process the wedge started.
        if unsafe { libc::kill(process.pid as i32, libc::SIGKILL) } != 0 {
            return Ok(false);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netem_args_are_numbers_only() {
        assert_eq!(Netem::Delay { latency_ms: 200, jitter_ms: 0, correlation_pct: 50 }.args(), ["delay", "200ms"]);
        assert_eq!(Netem::Delay { latency_ms: 200, jitter_ms: 20, correlation_pct: 50 }.args(), ["delay", "200ms", "20ms", "50%"]);
        assert_eq!(Netem::Loss { loss_pct: 10, correlation_pct: 0 }.args(), ["loss", "10%"]);
        assert_eq!(Netem::Loss { loss_pct: 10, correlation_pct: 25 }.args(), ["loss", "10%", "25%"]);
    }

    #[test]
    fn start_ticks_of_this_process() {
        assert!(process_start_ticks(Path::new("/proc"), std::process::id()).is_some());
        assert_eq!(process_start_ticks(Path::new("/proc"), u32::MAX), None);
    }
}
