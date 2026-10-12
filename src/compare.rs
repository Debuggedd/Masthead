//! Compares one object across clusters or namespaces, keeping only what you configure: spec/data and
//! your own labels and annotations. Status, server-set fields and per-copy noise are dropped, list
//! items are matched by name (a reorder isn't a change), secret values are compared by hash, and
//! cluster/namespace names can be masked so `api.staging.example.com` equals `api.prod.example.com`.

use regex::Regex;
use serde_json::Value;
use std::collections::BTreeMap;
use std::hash::{Hash, Hasher};

/// One side of a comparison: the object and where it lives.
pub struct Side<'a> {
    pub obj: &'a Value,
    pub ctx: &'a str,
    pub ns: &'a str,
}

#[derive(Debug, PartialEq)]
pub struct Change {
    pub path: String,
    pub a: Option<String>,
    pub b: Option<String>,
}

/// Paths set by the API server or controllers, or different in every copy by design.
const SERVER_SET: &[&str] = &[
    "spec.nodeName",
    "spec.clusterIP",
    "spec.clusterIPs",
    "spec.healthCheckNodePort",
    "spec.volumeName",
];

/// Label/annotation keys tools and controllers write.
const NOISY_KEYS: &[&str] = &[
    "kubectl.kubernetes.io/",
    "deployment.kubernetes.io/",
    "kustomize.toolkit.fluxcd.io/",
    "helm.toolkit.fluxcd.io/",
    "reconcile.fluxcd.io/",
    "meta.helm.sh/",
    "batch.kubernetes.io/",
    "controller-uid",
    "pod-template-hash",
    "pod-template-generation",
    "controller-revision-hash",
    "statefulset.kubernetes.io/pod-name",
];

pub const CLUSTER: &str = "‹cluster›";
pub const NAMESPACE: &str = "‹namespace›";

/// Differences between `a` and `b`, most telling first (images, replicas, resources, env, the rest).
pub fn compare(kind: &str, a: &Side, b: &Side, mask: bool) -> Vec<Change> {
    let (masks_a, masks_b) = if mask { (masks(a, b), masks(b, a)) } else { (vec![], vec![]) };
    let (fa, fb) = (fields(kind, a.obj, &masks_a), fields(kind, b.obj, &masks_b));
    let mut paths: Vec<&String> = fa.keys().chain(fb.keys()).collect();
    paths.sort_by_key(|p| (rank(p), p.as_str()));
    paths.dedup();
    paths
        .into_iter()
        .filter(|p| fa.get(*p) != fb.get(*p))
        .map(|p| Change {
            path: p.clone(),
            a: fa.get(p).cloned(),
            b: fb.get(p).cloned(),
        })
        .collect()
}

fn rank(path: &str) -> u8 {
    match () {
        _ if path.ends_with(".image") => 0,
        _ if path.ends_with("replicas") => 1,
        _ if path.contains(".resources.") => 2,
        _ if path.contains(".env[") => 3,
        _ => 4,
    }
}

/// The compared fields as "path → value": everything but metadata (except labels/annotations) and status.
fn fields(kind: &str, obj: &Value, masks: &[(Regex, &str)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (key, v) in obj.as_object().into_iter().flatten() {
        match key.as_str() {
            "apiVersion" | "kind" | "status" => {}
            "metadata" => {
                for part in ["labels", "annotations"] {
                    flatten(&v[part], format!("metadata.{part}"), &mut out);
                }
            }
            _ => flatten(v, key.clone(), &mut out),
        }
    }
    out.retain(|path, _| !noisy(path));
    for (path, value) in out.iter_mut() {
        if kind == "Secret" && (path.starts_with("data.") || path.starts_with("stringData.")) {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            value.hash(&mut h);
            *value = format!("•••••• #{:06x}", h.finish() & 0xff_ffff);
        } else {
            for (re, with) in masks {
                *value = re.replace_all(value, format!("${{a}}{with}${{z}}")).into_owned();
            }
        }
    }
    out
}

fn noisy(path: &str) -> bool {
    if SERVER_SET.iter().any(|p| path == *p || path.starts_with(&format!("{p}["))) {
        return true;
    }
    // A label/annotation anywhere (object, pod template, selector) whose key a tool owns.
    ["labels.", "annotations.", "matchLabels."].iter().any(|m| {
        path.split_once(m)
            .is_some_and(|(_, key)| NOISY_KEYS.iter().any(|n| key.starts_with(n)))
    })
}

/// Leaves as dotted paths; list items keyed by their `name` when they all have one.
fn flatten(v: &Value, path: String, out: &mut BTreeMap<String, String>) {
    match v {
        Value::Null => {}
        Value::Object(m) => {
            for (k, v) in m {
                flatten(v, format!("{path}.{k}"), out);
            }
        }
        Value::Array(items) => {
            let names: Option<Vec<&str>> = items.iter().map(|i| i.get("name").and_then(Value::as_str)).collect();
            for (i, item) in items.iter().enumerate() {
                let key = match &names {
                    // Service-account token volumes get a random suffix per pod.
                    Some(n) if n[i].starts_with("kube-api-access-") => "kube-api-access".to_string(),
                    Some(n) => n[i].to_string(),
                    None => i.to_string(),
                };
                flatten(item, format!("{path}[{key}]"), out);
            }
        }
        Value::String(s) => {
            out.insert(path, s.clone());
        }
        other => {
            out.insert(path, other.to_string());
        }
    }
}

/// Replacements for `side`'s own names (where they differ from `other`'s): the whole context and
/// namespace names, and the words only that side's name has ("prod" in "acme-prod" vs "acme-test").
fn masks(side: &Side, other: &Side) -> Vec<(Regex, &'static str)> {
    let mut words: Vec<(String, &'static str)> = vec![];
    for (mine, theirs, with) in [(side.ctx, other.ctx, CLUSTER), (side.ns, other.ns, NAMESPACE)] {
        if mine == theirs || mine.is_empty() {
            continue;
        }
        words.push((mine.to_string(), with));
        let split = |s: &str| {
            s.split(|c: char| !c.is_ascii_alphanumeric())
                .map(str::to_string)
                .collect::<Vec<_>>()
        };
        let theirs = split(theirs);
        words.extend(
            split(mine)
                .into_iter()
                .filter(|w| w.len() > 1 && !theirs.contains(w))
                .map(|w| (w, with)),
        );
    }
    words.sort_by_key(|(w, _)| std::cmp::Reverse(w.len()));
    words
        .into_iter()
        .filter_map(|(w, with)| {
            let re = Regex::new(&format!("(?P<a>^|[^A-Za-z0-9]){}(?P<z>$|[^A-Za-z0-9])", regex::escape(&w))).ok()?;
            Some((re, with))
        })
        .collect()
}

#[test]
fn compares_what_you_configure() {
    use serde_json::json;
    let deploy = |ctx: &str, image: &str, order: bool| {
        let (api, side) = (
            json!({"name": "api", "image": image, "env": [{"name": "HOST", "value": format!("api.{ctx}.example.com")}]}),
            json!({"name": "proxy", "image": "envoy:1"}),
        );
        json!({
            "apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "api", "uid": ctx, "resourceVersion": "1",
                "labels": {"app": "api", "kustomize.toolkit.fluxcd.io/name": ctx},
                "annotations": {"deployment.kubernetes.io/revision": ctx}},
            "spec": {"replicas": 2, "template": {
                "metadata": {"annotations": {"kubectl.kubernetes.io/restartedAt": ctx}},
                "spec": {"containers": if order { json!([api, side]) } else { json!([side, api]) }}}},
            "status": {"readyReplicas": 1},
        })
    };
    let (a, b) = (deploy("acme-staging", "api:1.0", true), deploy("acme-prod", "api:1.1", false));
    let side = |obj, ctx| Side { obj, ctx, ns: "shop" };
    let (a, b) = (side(&a, "acme-staging"), side(&b, "acme-prod"));
    // Masked: only the image differs (reordered containers, uid, revision, restartedAt, Flux label are noise).
    let changes = compare("Deployment", &a, &b, true);
    assert_eq!(
        changes,
        vec![Change {
            path: "spec.template.spec.containers[api].image".into(),
            a: Some("api:1.0".into()),
            b: Some("api:1.1".into()),
        }]
    );
    // Unmasked: the hostnames differ too.
    let changes = compare("Deployment", &a, &b, false);
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[1].path, "spec.template.spec.containers[api].env[HOST].value");

    // Secrets: values compared, never shown.
    let (sa, sb) = (
        json!({"data": {"pw": "YQ==", "user": "dQ=="}}),
        json!({"data": {"pw": "Yg==", "user": "dQ=="}}),
    );
    let changes = compare("Secret", &side(&sa, "a"), &side(&sb, "b"), true);
    assert_eq!(changes.len(), 1);
    assert!(changes[0].a.as_deref().unwrap().starts_with("••••••") && changes[0].a != changes[0].b);
}
