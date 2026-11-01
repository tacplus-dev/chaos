// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Resolving a target on this node, and stopping a container: the wedge's only use of the container
//! runtime. It talks CRI to containerd over its socket and makes exactly these calls (README.md):
//!
//! - `ListPodSandbox` (filter: the pod's namespace and name labels, state READY) — is the pod on this node;
//! - `PodSandboxStatus` (verbose) — the sandbox's pid, whose network namespace is the pod's;
//! - `ListContainers` (filter: that sandbox) — the pod's containers; the target is a running one;
//! - `ContainerStatus` (verbose), for each — the target's pid (for its cgroup) and every container's mounts
//!   (does the pod hold a Secret);
//! - `StopContainer` (timeout 0) — `CreateContainerKill`, the one call that changes anything.
//!
//! A socket mounted read-only still allows every runtime call: the code is the boundary, so no other
//! call is made, and none takes an id from the caller.

use std::fs;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tonic::transport::{Channel, Endpoint, Uri};

use crate::cri::runtime_service_client::RuntimeServiceClient;
use crate::cri::{
    ContainerFilter, ContainerState, ContainerStatusRequest, ListContainersRequest, ListPodSandboxRequest,
    PodSandboxFilter, PodSandboxState, PodSandboxStateValue, PodSandboxStatusRequest, StopContainerRequest,
};

/// A target as the wedge resolved it on its own node.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub namespace: String,
    pub pod: String,
    pub container: String,
    pub container_id: String,
    /// The pod's labels, as the runtime has them.
    pub service: String,
    /// The sandbox's pid: `/proc/<pid>/ns/net` is the pod's network namespace.
    pub sandbox_pid: u32,
    /// The container's cgroup directory (cgroup v2).
    pub cgroup_dir: PathBuf,
    pub cpu_limit_millis: Option<u64>,
    pub memory_limit_bytes: Option<u64>,
    /// Whether any container of the pod mounts a Kubernetes Secret volume.
    pub mounts_secret: bool,
}

/// The node's container runtime, as the wedge uses it.
#[async_trait]
pub trait Runtime: Send + Sync + 'static {
    /// Resolves namespace/pod/container on this node; Err (the reason) when it is not here.
    async fn resolve(&self, namespace: &str, pod: &str, container: &str) -> Result<Resolved, String>;
    /// Stops a container at once (SIGKILL): the kubelet restarts it.
    async fn stop_container(&self, container_id: &str) -> Result<(), String>;
}

/// containerd, through CRI.
pub struct Cri {
    channel: Channel,
    /// The host's /proc (hostPID) and cgroup v2 mount.
    proc_root: PathBuf,
    cgroup_root: PathBuf,
}

impl Cri {
    /// Connects to the runtime's socket (`/run/containerd/containerd.sock`).
    pub async fn connect(socket: &Path) -> Result<Cri, Box<dyn std::error::Error>> {
        let socket = socket.to_path_buf();
        let channel = Endpoint::try_from("http://[::]:50051")?
            .connect_with_connector(tower::service_fn(move |_: Uri| {
                let socket = socket.clone();
                async move { Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(tokio::net::UnixStream::connect(socket).await?)) }
            }))
            .await?;
        Ok(Cri { channel, proc_root: PathBuf::from("/proc"), cgroup_root: PathBuf::from("/sys/fs/cgroup") })
    }

    fn client(&self) -> RuntimeServiceClient<Channel> {
        RuntimeServiceClient::new(self.channel.clone())
    }
}

/// The `pid` of a verbose status's `info` JSON.
fn info_pid(info: &std::collections::HashMap<String, String>) -> Option<u32> {
    let v: serde_json::Value = serde_json::from_str(info.get("info")?).ok()?;
    v.get("pid")?.as_u64().map(|p| p as u32)
}

/// Whether a mount's host path is a volume that can hold a Secret: a `secret` volume, a `projected` one
/// other than the service account token's (`kube-api-access-*`, which every pod has), or any CSI volume
/// (a secrets-store driver). Conservative on purpose: a pod wrongly excluded is safe.
pub fn holds_secret(host_path: &str) -> bool {
    if host_path.contains("/volumes/kubernetes.io~secret/") || host_path.contains("/volumes/kubernetes.io~csi/") {
        return true;
    }
    match host_path.split_once("/volumes/kubernetes.io~projected/") {
        Some((_, rest)) => !rest.starts_with("kube-api-access-"),
        None => false,
    }
}

/// The cgroup v2 directory of a process, from `/proc/<pid>/cgroup` (`0::/kubepods/...`).
pub fn cgroup_dir(proc_root: &Path, cgroup_root: &Path, pid: u32) -> Option<PathBuf> {
    let text = fs::read_to_string(proc_root.join(pid.to_string()).join("cgroup")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(cgroup_root.join(path.trim_start_matches('/')))
}

/// A cgroup's CPU limit in millicores (`cpu.max` "quota period"; "max" is none).
pub fn cpu_limit_millis(dir: &Path) -> Option<u64> {
    let text = fs::read_to_string(dir.join("cpu.max")).ok()?;
    let mut it = text.split_whitespace();
    let (quota, period) = (it.next()?.parse::<u64>().ok()?, it.next()?.parse::<u64>().ok()?);
    (period > 0).then(|| quota * 1000 / period)
}

/// A cgroup's memory limit in bytes (`memory.max`; "max" is none).
pub fn memory_limit_bytes(dir: &Path) -> Option<u64> {
    fs::read_to_string(dir.join("memory.max")).ok()?.trim().parse().ok()
}

#[async_trait]
impl Runtime for Cri {
    async fn resolve(&self, namespace: &str, pod: &str, container: &str) -> Result<Resolved, String> {
        let mut client = self.client();
        let sandboxes = client
            .list_pod_sandbox(ListPodSandboxRequest {
                filter: Some(PodSandboxFilter {
                    state: Some(PodSandboxStateValue { state: PodSandboxState::SandboxReady as i32 }),
                    label_selector: [("io.kubernetes.pod.namespace".to_string(), namespace.to_string()), ("io.kubernetes.pod.name".to_string(), pod.to_string())].into(),
                    ..Default::default()
                }),
            })
            .await
            .map_err(|e| format!("ListPodSandbox: {e}"))?
            .into_inner()
            .items;
        let [sandbox] = &sandboxes[..] else {
            return Err(format!("pod {namespace}/{pod} is not running on this node"));
        };
        let status = client
            .pod_sandbox_status(PodSandboxStatusRequest { pod_sandbox_id: sandbox.id.clone(), verbose: true })
            .await
            .map_err(|e| format!("PodSandboxStatus: {e}"))?
            .into_inner();
        let sandbox_pid = info_pid(&status.info).ok_or("the runtime gave no sandbox pid")?;
        let containers = client
            .list_containers(ListContainersRequest {
                filter: Some(ContainerFilter {
                    // Every container of the pod, running or not: one that mounts a Secret excludes the pod
                    // even while it is restarting.
                    pod_sandbox_id: sandbox.id.clone(),
                    ..Default::default()
                }),
            })
            .await
            .map_err(|e| format!("ListContainers: {e}"))?
            .into_inner()
            .containers;
        let mut mounts_secret = false;
        let mut chosen = None;
        for c in &containers {
            let name = c.metadata.as_ref().map(|m| m.name.clone()).unwrap_or_default();
            let st = client
                .container_status(ContainerStatusRequest { container_id: c.id.clone(), verbose: true })
                .await
                .map_err(|e| format!("ContainerStatus: {e}"))?
                .into_inner();
            if let Some(s) = &st.status {
                mounts_secret |= s.mounts.iter().any(|m| holds_secret(&m.host_path));
            }
            let running = c.state == ContainerState::ContainerRunning as i32;
            if running && chosen.is_none() && (container.is_empty() || name == container) {
                chosen = Some((name, c.id.clone(), info_pid(&st.info)));
            }
        }
        let Some((name, container_id, Some(pid))) = chosen else {
            return Err(format!("container {container:?} of pod {namespace}/{pod} is not running"));
        };
        let cgroup_dir = cgroup_dir(&self.proc_root, &self.cgroup_root, pid).ok_or("no cgroup v2 path for the container")?;
        Ok(Resolved {
            namespace: namespace.into(),
            pod: pod.into(),
            container: name,
            container_id,
            service: sandbox.labels.get(chaos_policy::exclusion::SERVICE_LABEL).cloned().unwrap_or_default(),
            sandbox_pid,
            cpu_limit_millis: cpu_limit_millis(&cgroup_dir),
            memory_limit_bytes: memory_limit_bytes(&cgroup_dir),
            cgroup_dir,
            mounts_secret,
        })
    }

    async fn stop_container(&self, container_id: &str) -> Result<(), String> {
        self.client()
            .stop_container(StopContainerRequest { container_id: container_id.into(), timeout: 0 })
            .await
            .map(|_| ())
            .map_err(|e| format!("StopContainer: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_limits() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("cpu.max"), "150000 100000\n").unwrap();
        fs::write(dir.path().join("memory.max"), "268435456\n").unwrap();
        assert_eq!(cpu_limit_millis(dir.path()), Some(1500));
        assert_eq!(memory_limit_bytes(dir.path()), Some(268435456));
        fs::write(dir.path().join("cpu.max"), "max 100000\n").unwrap();
        fs::write(dir.path().join("memory.max"), "max\n").unwrap();
        assert_eq!(cpu_limit_millis(dir.path()), None);
        assert_eq!(memory_limit_bytes(dir.path()), None);
    }

    #[test]
    fn cgroup_dir_of_a_process() {
        let proc_root = tempfile::tempdir().unwrap();
        fs::create_dir_all(proc_root.path().join("42")).unwrap();
        fs::write(proc_root.path().join("42/cgroup"), "0::/kubepods.slice/pod1.slice/cri-containerd-abc.scope\n").unwrap();
        assert_eq!(
            cgroup_dir(proc_root.path(), Path::new("/sys/fs/cgroup"), 42),
            Some(PathBuf::from("/sys/fs/cgroup/kubepods.slice/pod1.slice/cri-containerd-abc.scope"))
        );
    }

    #[test]
    fn secret_volumes() {
        let v = "/var/lib/kubelet/pods/u1/volumes";
        assert!(holds_secret(&format!("{v}/kubernetes.io~secret/tls")));
        assert!(holds_secret(&format!("{v}/kubernetes.io~projected/bundle")));
        assert!(holds_secret(&format!("{v}/kubernetes.io~csi/vault/mount")));
        assert!(!holds_secret(&format!("{v}/kubernetes.io~projected/kube-api-access-x7k2p")));
        assert!(!holds_secret(&format!("{v}/kubernetes.io~configmap/config")));
        assert!(!holds_secret("/var/lib/kubelet/pods/u1/etc-hosts"));
    }

    #[test]
    fn verbose_info_pid() {
        let info = [("info".to_string(), r#"{"pid": 1234, "runtimeSpec": {}}"#.to_string())].into();
        assert_eq!(info_pid(&info), Some(1234));
        assert_eq!(info_pid(&Default::default()), None);
    }
}
