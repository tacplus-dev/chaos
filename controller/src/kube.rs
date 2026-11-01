// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The controller's Kubernetes API calls, all in its fleet's namespaces (its Role allows nothing else):
//! list pods, delete one (the pod kill), and create an Event on the target (the fault's marker, so
//! `kubectl describe` shows it, fleet-seatrials#26).

use async_trait::async_trait;
use chaos_policy::exclusion::SERVICE_LABEL;

/// A pod, as the controller needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PodInfo {
    pub namespace: String,
    pub name: String,
    /// Its tacplus service (app.kubernetes.io/name); empty when unlabelled.
    pub service: String,
    pub node: String,
    pub ip: String,
    pub running: bool,
    pub ready: bool,
    /// Whether it mounts a Secret volume (secret material: never a target).
    pub mounts_secret: bool,
}

#[async_trait]
pub trait Cluster: Send + Sync + 'static {
    async fn list_pods(&self, namespace: &str) -> Result<Vec<PodInfo>, String>;
    async fn delete_pod(&self, namespace: &str, name: &str, grace_period_s: u32) -> Result<(), String>;
    /// A Warning Event on the pod, reason `ChaosFault`, with the fault's tag in its message.
    async fn create_event(&self, namespace: &str, pod: &str, message: &str) -> Result<(), String>;
}

/// Parses a PodList's items.
pub fn parse_pods(namespace: &str, list: &serde_json::Value) -> Vec<PodInfo> {
    let Some(items) = list["items"].as_array() else { return Vec::new() };
    items
        .iter()
        .map(|p| PodInfo {
            namespace: namespace.into(),
            name: p["metadata"]["name"].as_str().unwrap_or_default().into(),
            service: p["metadata"]["labels"][SERVICE_LABEL].as_str().unwrap_or_default().into(),
            node: p["spec"]["nodeName"].as_str().unwrap_or_default().into(),
            ip: p["status"]["podIP"].as_str().unwrap_or_default().into(),
            running: p["status"]["phase"] == "Running" && p["metadata"]["deletionTimestamp"].is_null(),
            ready: p["status"]["conditions"].as_array().is_some_and(|cs| cs.iter().any(|c| c["type"] == "Ready" && c["status"] == "True")),
            mounts_secret: p["spec"]["volumes"].as_array().is_some_and(|vs| vs.iter().any(|v| v.get("secret").is_some())),
        })
        .collect()
}

const SA_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

/// The in-cluster API server, with the pod's service account.
pub struct InCluster {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl InCluster {
    pub fn from_env() -> Result<InCluster, Box<dyn std::error::Error>> {
        let host = std::env::var("KUBERNETES_SERVICE_HOST")?;
        let port = std::env::var("KUBERNETES_SERVICE_PORT")?;
        let ca = reqwest::Certificate::from_pem(&std::fs::read(format!("{SA_DIR}/ca.crt"))?)?;
        let token = std::fs::read_to_string(format!("{SA_DIR}/token"))?.trim().to_string();
        let http = reqwest::Client::builder().add_root_certificate(ca).build()?;
        Ok(InCluster { http, base: format!("https://{host}:{port}"), token })
    }

    async fn check(resp: reqwest::Response, what: &str) -> Result<reqwest::Response, String> {
        if resp.status().is_success() {
            Ok(resp)
        } else {
            Err(format!("{what}: {} {}", resp.status(), resp.text().await.unwrap_or_default()))
        }
    }
}

#[async_trait]
impl Cluster for InCluster {
    async fn list_pods(&self, namespace: &str) -> Result<Vec<PodInfo>, String> {
        let what = format!("list pods in {namespace}");
        let resp = self.http.get(format!("{}/api/v1/namespaces/{namespace}/pods", self.base)).bearer_auth(&self.token).send().await.map_err(|e| format!("{what}: {e}"))?;
        let list: serde_json::Value = Self::check(resp, &what).await?.json().await.map_err(|e| format!("{what}: {e}"))?;
        Ok(parse_pods(namespace, &list))
    }

    async fn delete_pod(&self, namespace: &str, name: &str, grace_period_s: u32) -> Result<(), String> {
        let what = format!("delete pod {namespace}/{name}");
        let resp = self
            .http
            .delete(format!("{}/api/v1/namespaces/{namespace}/pods/{name}", self.base))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({"gracePeriodSeconds": grace_period_s}))
            .send()
            .await
            .map_err(|e| format!("{what}: {e}"))?;
        Self::check(resp, &what).await.map(|_| ())
    }

    async fn create_event(&self, namespace: &str, pod: &str, message: &str) -> Result<(), String> {
        let what = format!("create event on {namespace}/{pod}");
        let now = chrono_like_now();
        let event = serde_json::json!({
            "metadata": {"generateName": "chaos-", "namespace": namespace},
            "involvedObject": {"kind": "Pod", "namespace": namespace, "name": pod},
            "reason": "ChaosFault",
            "message": message,
            "type": "Warning",
            "source": {"component": chaos_policy::CONTROLLER},
            "firstTimestamp": now,
            "lastTimestamp": now,
            "count": 1,
        });
        let resp = self.http.post(format!("{}/api/v1/namespaces/{namespace}/events", self.base)).bearer_auth(&self.token).json(&event).send().await.map_err(|e| format!("{what}: {e}"))?;
        Self::check(resp, &what).await.map(|_| ())
    }
}

/// Now as RFC 3339 UTC (seconds), for an Event's timestamps.
fn chrono_like_now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // Civil from days (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pods_from_a_pod_list() {
        let list = serde_json::json!({"items": [
            {"metadata": {"name": "stratum-0", "labels": {"app.kubernetes.io/name": "stratum"}},
             "spec": {"nodeName": "n1", "volumes": [{"name": "kube-api-access", "projected": {}}]},
             "status": {"phase": "Running", "podIP": "10.0.0.5", "conditions": [{"type": "Ready", "status": "True"}]}},
            {"metadata": {"name": "kmaker-0", "labels": {"app.kubernetes.io/name": "kmaker"}, "deletionTimestamp": "2026-11-01T00:00:00Z"},
             "spec": {"nodeName": "n2", "volumes": [{"name": "keys", "secret": {"secretName": "kmaker"}}]},
             "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "False"}]}}
        ]});
        let pods = parse_pods("tac-prod", &list);
        assert_eq!(pods[0], PodInfo { namespace: "tac-prod".into(), name: "stratum-0".into(), service: "stratum".into(), node: "n1".into(), ip: "10.0.0.5".into(), running: true, ready: true, mounts_secret: false });
        assert!(!pods[1].running, "a pod being deleted is not a target");
        assert!(!pods[1].ready);
        assert!(pods[1].mounts_secret);
    }

    #[test]
    fn event_timestamps() {
        let now = chrono_like_now();
        assert_eq!(now.len(), 20);
        assert!(now.starts_with("20") && now.ends_with('Z'));
    }
}
