// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! The wedge's one Kubernetes API read: a namespace's labels, to know which fleet it belongs to (nsproxy's
//! labels, never the name). Its ClusterRole allows `get namespaces` and nothing else.

use std::collections::BTreeMap;

use async_trait::async_trait;

#[async_trait]
pub trait Namespaces: Send + Sync + 'static {
    async fn labels(&self, namespace: &str) -> Result<BTreeMap<String, String>, String>;
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
}

#[async_trait]
impl Namespaces for InCluster {
    async fn labels(&self, namespace: &str) -> Result<BTreeMap<String, String>, String> {
        if namespace.is_empty() || !namespace.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err(format!("{namespace:?} is not a namespace name"));
        }
        let resp = self
            .http
            .get(format!("{}/api/v1/namespaces/{namespace}", self.base))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(|e| format!("get namespace {namespace}: {e}"))?;
        if !resp.status().is_success() {
            return Err(format!("get namespace {namespace}: {}", resp.status()));
        }
        let v: serde_json::Value = resp.json().await.map_err(|e| format!("namespace {namespace}: {e}"))?;
        Ok(v["metadata"]["labels"]
            .as_object()
            .map(|m| m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect())
            .unwrap_or_default())
    }
}
