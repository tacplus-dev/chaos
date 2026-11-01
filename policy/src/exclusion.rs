// Copyright (c) 2026 Jody Scott
// SPDX-License-Identifier: Apache-2.0

//! Which pods a fault may target (#1, design section 2): only a pod of the caller's own fleet, and never
//! an excluded one. The fleet is read from the namespace's own labels, as nsproxy sets them, never
//! inferred from its name.

use std::collections::BTreeMap;

/// The label the pipeline puts on a deployment namespace (nsproxy README).
pub const DEPLOYMENT_LABEL: &str = "tacplus.dev/deployment";
/// The label nsproxy puts on each tenant namespace it owns, naming the deployment.
pub const TENANT_DEPLOYMENT_LABEL: &str = "nsproxy.tacplus.dev/deployment";
/// The pod label naming its tacplus service.
pub const SERVICE_LABEL: &str = "app.kubernetes.io/name";

/// Services no fault ever targets: certificate issuance and the chaos services themselves.
pub const EXCLUDED_SERVICES: &[&str] = &["letsencrypt", crate::CONTROLLER, crate::WEDGE];
/// Namespaces no fault ever targets, beyond every `kube-*` one. A namespace outside the fleet is refused
/// anyway; these hold even if one is mislabelled.
pub const EXCLUDED_NAMESPACES: &[&str] = &["default", "nsproxy-system", "letsencrypt"];

/// What a check knows about a pod, from the side doing the check (the wedge: its runtime and the
/// Kubernetes API's namespace labels; the controller: the pod list).
#[derive(Clone, Debug, Default)]
pub struct PodFacts {
    pub namespace: String,
    pub namespace_labels: BTreeMap<String, String>,
    pub pod: String,
    /// The pod's tacplus service (its app.kubernetes.io/name label); empty when unlabelled.
    pub service: String,
    /// Whether any of its containers mounts a Kubernetes Secret volume: the fleet's secret material (the
    /// uni-config Secret, the PKI, any key) lives only in Secrets, so such a pod is never a target.
    pub mounts_secret: bool,
}

/// The exclusion list in force: the compiled-in one plus the configured additions.
#[derive(Clone, Debug, Default)]
pub struct Exclusions {
    pub namespaces: Vec<String>,
    pub services: Vec<String>,
}

impl Exclusions {
    pub fn with(namespaces: &[String], services: &[String]) -> Exclusions {
        Exclusions { namespaces: namespaces.to_vec(), services: services.to_vec() }
    }

    fn namespace_excluded(&self, namespace: &str) -> bool {
        namespace.starts_with("kube-")
            || EXCLUDED_NAMESPACES.contains(&namespace)
            || self.namespaces.iter().any(|n| n == namespace)
    }

    fn service_excluded(&self, service: &str) -> bool {
        EXCLUDED_SERVICES.contains(&service) || self.services.iter().any(|s| s == service)
    }

    /// Refuses an excluded namespace by its name alone, before anything is looked up.
    pub fn check_namespace(&self, namespace: &str) -> Result<(), String> {
        if self.namespace_excluded(namespace) {
            return Err(format!("namespace {namespace} is excluded"));
        }
        Ok(())
    }

    /// Ok when a fault of `fleet` may target the pod; else why not.
    pub fn check(&self, fleet: &str, pod: &PodFacts) -> Result<(), String> {
        if fleet.is_empty() {
            return Err("no fleet is configured".into());
        }
        self.check_namespace(&pod.namespace)?;
        if !in_fleet(fleet, &pod.namespace_labels) {
            return Err(format!("namespace {} is not in fleet {fleet}", pod.namespace));
        }
        if pod.service.is_empty() {
            return Err(format!("pod {}/{} has no {SERVICE_LABEL} label", pod.namespace, pod.pod));
        }
        if self.service_excluded(&pod.service) {
            return Err(format!("service {} is excluded", pod.service));
        }
        if pod.mounts_secret {
            return Err(format!("pod {}/{} mounts secret material", pod.namespace, pod.pod));
        }
        Ok(())
    }
}

/// Whether a namespace with these labels belongs to `fleet`: its deployment namespace, or one of the tenant
/// namespaces nsproxy made for it.
pub fn in_fleet(fleet: &str, labels: &BTreeMap<String, String>) -> bool {
    labels.get(DEPLOYMENT_LABEL).is_some_and(|d| d == fleet) || labels.get(TENANT_DEPLOYMENT_LABEL).is_some_and(|d| d == fleet)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pod(namespace: &str, label: (&str, &str), service: &str) -> PodFacts {
        PodFacts {
            namespace: namespace.into(),
            namespace_labels: BTreeMap::from([(label.0.to_string(), label.1.to_string())]),
            pod: format!("{service}-0"),
            service: service.into(),
            mounts_secret: false,
        }
    }

    #[test]
    fn own_fleet_pods_pass() {
        let ex = Exclusions::default();
        assert_eq!(ex.check("prod", &pod("tac-prod", (DEPLOYMENT_LABEL, "prod"), "stratum")), Ok(()));
        assert_eq!(ex.check("prod", &pod("tac-prod-a1b2c3d4", (TENANT_DEPLOYMENT_LABEL, "prod"), "people")), Ok(()));
    }

    #[test]
    fn another_fleet_is_refused() {
        let ex = Exclusions::default();
        let err = ex.check("prod", &pod("tac-dev", (DEPLOYMENT_LABEL, "dev"), "stratum")).unwrap_err();
        assert!(err.contains("not in fleet prod"), "{err}");
        // A namespace named like the fleet's but without its label is not the fleet's.
        let unlabelled = PodFacts { namespace: "tac-prod".into(), service: "stratum".into(), ..Default::default() };
        assert!(ex.check("prod", &unlabelled).unwrap_err().contains("not in fleet"));
    }

    #[test]
    fn excluded_targets_are_refused() {
        let ex = Exclusions::with(&["tac-prod-quiet".into()], &["kernel".into()]);
        let lbl = (DEPLOYMENT_LABEL, "prod");
        assert!(ex.check("prod", &pod("tac-prod", lbl, "letsencrypt")).unwrap_err().contains("letsencrypt is excluded"));
        assert!(ex.check("prod", &pod("tac-prod", lbl, "chaos-wedge")).unwrap_err().contains("excluded"));
        assert!(ex.check("prod", &pod("tac-prod", lbl, "chaos-controller")).unwrap_err().contains("excluded"));
        assert!(ex.check("prod", &pod("tac-prod", lbl, "kernel")).unwrap_err().contains("kernel is excluded"));
        assert!(ex.check("prod", &pod("kube-system", lbl, "coredns")).unwrap_err().contains("kube-system is excluded"));
        assert!(ex.check("prod", &pod("tac-prod-quiet", lbl, "stratum")).unwrap_err().contains("excluded"));
        let mut secret = pod("tac-prod", lbl, "kmaker");
        secret.mounts_secret = true;
        assert!(ex.check("prod", &secret).unwrap_err().contains("secret material"));
        assert!(ex.check("prod", &pod("tac-prod", lbl, "")).unwrap_err().contains("no app.kubernetes.io/name"));
        assert!(ex.check("", &pod("tac-prod", lbl, "stratum")).is_err());
    }
}
