use anyhow::Result;
use futures::StreamExt;
use k8s_openapi::{api::core::v1::Pod, apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition, jiff::Timestamp};
use kube::{
    Api, Client, Config,
    api::{ApiResource, DeleteParams, DynamicObject, EvictParams, ListParams, LogParams, Patch, PatchParams, PostParams},
    config::{KubeConfigOptions, Kubeconfig},
    runtime::{WatchStreamExt, watcher, watcher::Event},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::{
        Arc, LazyLock, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

/// kube needs tokio; GPUI has its own executor. GPUI tasks just await tokio JoinHandles.
pub static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| tokio::runtime::Runtime::new().expect("tokio runtime"));

/// One shared client (one connection pool) per context; concurrent first callers wait for the same init.
static CLIENTS: LazyLock<Mutex<HashMap<Ctx, Arc<tokio::sync::OnceCell<Client>>>>> = LazyLock::new(Default::default);

/// Live cache filled by watchers: (context, kind index) -> slot.
pub static CACHE: LazyLock<Mutex<HashMap<(Ctx, usize), Slot>>> = LazyLock::new(Default::default);
/// Bumped on every cache change so the UI knows when to re-read.
pub static VERSION: AtomicU64 = AtomicU64::new(0);

/// Latest metrics-server snapshot per context (polled; the metrics API can't be watched).
pub static METRICS: LazyLock<Mutex<HashMap<Ctx, Arc<Metrics>>>> = LazyLock::new(Default::default);

/// CPU in cores, memory in bytes.
#[derive(Default, Clone, Copy, PartialEq, Debug)]
pub struct Usage {
    pub cpu: f64,
    pub mem: f64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.cpu += o.cpu;
        self.mem += o.mem;
    }
}

/// Recent usage per pod and node (one sample per metrics poll), for sparklines.
pub static HISTORY: LazyLock<Mutex<HashMap<Ctx, History>>> = LazyLock::new(Default::default);
/// 60 samples × 15s = the last 15 minutes.
pub const HISTORY_LEN: usize = 60;

#[derive(Default)]
pub struct History {
    pub pods: HashMap<(String, String), std::collections::VecDeque<Usage>>,
    pub nodes: HashMap<String, std::collections::VecDeque<Usage>>,
}

impl History {
    fn record(&mut self, m: &Metrics) {
        fn push<K: Eq + std::hash::Hash + Clone>(series: &mut HashMap<K, std::collections::VecDeque<Usage>>, now: Vec<(K, Usage)>) {
            let live: std::collections::HashSet<K> = now.iter().map(|(k, _)| k.clone()).collect();
            series.retain(|k, _| live.contains(k)); // forget pods/nodes that are gone
            for (k, u) in now {
                let s = series.entry(k).or_default();
                if s.len() == HISTORY_LEN {
                    s.pop_front();
                }
                s.push_back(u);
            }
        }
        push(&mut self.pods, m.pods.iter().map(|(k, (u, _))| (k.clone(), *u)).collect());
        push(&mut self.nodes, m.nodes.iter().map(|(k, u)| (k.clone(), *u)).collect());
    }
}

#[derive(Default)]
pub struct Metrics {
    /// (namespace, pod) -> total usage and per-container usage.
    pub pods: HashMap<(String, String), (Usage, Vec<(String, Usage)>)>,
    pub nodes: HashMap<String, Usage>,
}

#[derive(Default)]
pub struct Slot {
    pub rows: BTreeMap<(String, String), Arc<Row>>,
    pub synced: bool,
    pub error: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Ctx {
    pub name: String,
    pub file: PathBuf,
}

pub struct Kind {
    pub name: &'static str,
    /// Lucide icon name in assets/icons.
    pub icon: &'static str,
    pub group: &'static str,
    version: &'static str,
    pub kind: &'static str,
    pub plural: &'static str,
    pub namespaced: bool,
    pub cols: &'static [&'static str],
    /// CRDs only: (column, JSONPath) from additionalPrinterColumns, aligned with `cols`.
    pub printer: &'static [(&'static str, &'static str)],
    /// A discovered CRD rather than one of the built-in kinds.
    pub custom: bool,
    /// Built-ins that are only watched once their page is opened (rarely used, or many objects).
    pub lazy: bool,
}

const fn k(
    name: &'static str,
    icon: &'static str,
    group: &'static str,
    version: &'static str,
    kind: &'static str,
    plural: &'static str,
    namespaced: bool,
    cols: &'static [&'static str],
) -> Kind {
    Kind {
        name,
        icon,
        group,
        version,
        kind,
        plural,
        namespaced,
        cols,
        printer: &[],
        custom: false,
        lazy: false,
    }
}

/// A built-in kind that's only watched once its page is opened.
const fn lazy(
    name: &'static str,
    icon: &'static str,
    group: &'static str,
    version: &'static str,
    kind: &'static str,
    plural: &'static str,
    namespaced: bool,
    cols: &'static [&'static str],
) -> Kind {
    Kind {
        lazy: true,
        ..k(name, icon, group, version, kind, plural, namespaced, cols)
    }
}

/// Built-in kinds; sidebar order, most used first within each group (see GROUPS in main.rs).
/// CRDs are appended at runtime by `discover`, so kind indices stay stable once handed out.
const BUILTIN: &[Kind] = &[
    k("Nodes", "cpu", "", "v1", "Node", "nodes", false, &["Status", "Roles"]),
    k(
        "Events",
        "activity",
        "",
        "v1",
        "Event",
        "events",
        true,
        &["Type", "Reason", "Object", "Message"],
    ),
    k("Namespaces", "folder", "", "v1", "Namespace", "namespaces", false, &["Status"]),
    k(
        "Pods",
        "box",
        "",
        "v1",
        "Pod",
        "pods",
        true,
        &["Ready", "Status", "Restarts", "Node"],
    ),
    k(
        "Deployments",
        "rocket",
        "apps",
        "v1",
        "Deployment",
        "deployments",
        true,
        &["Ready", "Up-to-date", "Available"],
    ),
    k(
        "StatefulSets",
        "database",
        "apps",
        "v1",
        "StatefulSet",
        "statefulsets",
        true,
        &["Ready"],
    ),
    k(
        "DaemonSets",
        "copy",
        "apps",
        "v1",
        "DaemonSet",
        "daemonsets",
        true,
        &["Desired", "Ready"],
    ),
    k(
        "CronJobs",
        "clock",
        "batch",
        "v1",
        "CronJob",
        "cronjobs",
        true,
        &["Schedule", "Suspend", "Active"],
    ),
    k("Jobs", "play", "batch", "v1", "Job", "jobs", true, &["Completions", "Status"]),
    lazy(
        "ReplicaSets",
        "boxes",
        "apps",
        "v1",
        "ReplicaSet",
        "replicasets",
        true,
        &["Desired", "Current", "Ready"],
    ),
    k(
        "Services",
        "share-2",
        "",
        "v1",
        "Service",
        "services",
        true,
        &["Type", "Cluster IP", "Ports"],
    ),
    k(
        "Ingresses",
        "globe",
        "networking.k8s.io",
        "v1",
        "Ingress",
        "ingresses",
        true,
        &["Class", "Hosts"],
    ),
    lazy(
        "NetworkPolicies",
        "brick-wall",
        "networking.k8s.io",
        "v1",
        "NetworkPolicy",
        "networkpolicies",
        true,
        &["Pod selector", "Policy types"],
    ),
    k("ConfigMaps", "file-text", "", "v1", "ConfigMap", "configmaps", true, &["Keys"]),
    k("Secrets", "key-round", "", "v1", "Secret", "secrets", true, &["Type", "Keys"]),
    // Watched up front: Scale checks whether an autoscaler owns the replica count.
    k(
        "HPAs",
        "gauge",
        "autoscaling",
        "v2",
        "HorizontalPodAutoscaler",
        "horizontalpodautoscalers",
        true,
        &["Target", "Metrics", "Min", "Max", "Replicas"],
    ),
    k(
        "PDBs",
        "shield",
        "policy",
        "v1",
        "PodDisruptionBudget",
        "poddisruptionbudgets",
        true,
        &["Min available", "Max unavailable", "Allowed disruptions"],
    ),
    lazy(
        "ResourceQuotas",
        "chart-pie",
        "",
        "v1",
        "ResourceQuota",
        "resourcequotas",
        true,
        &["Used / hard"],
    ),
    lazy("LimitRanges", "ruler", "", "v1", "LimitRange", "limitranges", true, &["Types"]),
    k(
        "PVCs",
        "hard-drive",
        "",
        "v1",
        "PersistentVolumeClaim",
        "persistentvolumeclaims",
        true,
        &["Status", "Capacity", "Class"],
    ),
    lazy(
        "PVs",
        "cylinder",
        "",
        "v1",
        "PersistentVolume",
        "persistentvolumes",
        false,
        &["Status", "Capacity", "Claim", "Class", "Reclaim"],
    ),
    lazy(
        "StorageClasses",
        "warehouse",
        "storage.k8s.io",
        "v1",
        "StorageClass",
        "storageclasses",
        false,
        &["Provisioner", "Reclaim", "Binding", "Default"],
    ),
    lazy(
        "ServiceAccounts",
        "bot",
        "",
        "v1",
        "ServiceAccount",
        "serviceaccounts",
        true,
        &["Secrets"],
    ),
    lazy(
        "Roles",
        "id-card",
        "rbac.authorization.k8s.io",
        "v1",
        "Role",
        "roles",
        true,
        &["Rules"],
    ),
    lazy(
        "RoleBindings",
        "link",
        "rbac.authorization.k8s.io",
        "v1",
        "RoleBinding",
        "rolebindings",
        true,
        &["Role", "Subjects"],
    ),
    lazy(
        "ClusterRoles",
        "badge-check",
        "rbac.authorization.k8s.io",
        "v1",
        "ClusterRole",
        "clusterroles",
        false,
        &["Rules"],
    ),
    lazy(
        "ClusterRoleBindings",
        "link-2",
        "rbac.authorization.k8s.io",
        "v1",
        "ClusterRoleBinding",
        "clusterrolebindings",
        false,
        &["Role", "Subjects"],
    ),
];

// ponytail: registry only grows (leaked CRD kinds); bounded by the number of distinct CRDs you browse.
static REGISTRY: LazyLock<RwLock<Vec<&'static Kind>>> = LazyLock::new(|| RwLock::new(BUILTIN.iter().collect()));

pub fn kind(ix: usize) -> &'static Kind {
    REGISTRY.read().unwrap()[ix]
}

pub fn kinds() -> Vec<&'static Kind> {
    REGISTRY.read().unwrap().clone()
}

pub fn find_kind(f: impl Fn(&Kind) -> bool) -> Option<usize> {
    REGISTRY.read().unwrap().iter().position(|k| f(k))
}

pub const BUILTIN_COUNT: usize = BUILTIN.len();

impl Kind {
    pub fn is_flux(&self) -> bool {
        self.group.ends_with(".toolkit.fluxcd.io")
    }

    /// Cluster-wide when `ns` is empty.
    fn api(&self, client: Client, ns: &str) -> Api<DynamicObject> {
        let api_version = match self.group {
            "" => self.version.to_string(),
            g => format!("{g}/{}", self.version),
        };
        let ar = ApiResource {
            group: self.group.into(),
            version: self.version.into(),
            api_version,
            kind: self.kind.into(),
            plural: self.plural.into(),
        };
        if ns.is_empty() {
            Api::all_with(client, &ar)
        } else {
            Api::namespaced_with(client, ns, &ar)
        }
    }
}

pub struct Row {
    pub ctx: Ctx,
    pub ns: String,
    pub name: String,
    pub cells: Vec<String>,
    /// Creation time (seconds), except Events: when last seen, like `kubectl get events`.
    pub created: i64,
    pub obj: Value,
}

/// Default kubeconfig ($KUBECONFIG or ~/.kube/config) plus user-added files.
pub fn kubeconfig_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = match std::env::var_os("KUBECONFIG") {
        Some(v) => std::env::split_paths(&v).collect(),
        None => dirs::home_dir().map(|h| h.join(".kube/config")).into_iter().collect(),
    };
    for f in std::fs::read_to_string(saved_list()).unwrap_or_default().lines() {
        let p = PathBuf::from(f.trim());
        if !f.trim().is_empty() && !files.contains(&p) {
            files.push(p);
        }
    }
    files
}

fn saved_list() -> PathBuf {
    app_dir().join("kubeconfigs")
}

/// Settings folder, ~/Library/Application Support/Masthead.
pub fn app_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_default().join("Masthead")
}

/// Kubeconfig files added in the app (not the $KUBECONFIG / ~/.kube/config defaults).
pub fn added_kubeconfig_files() -> Vec<PathBuf> {
    std::fs::read_to_string(saved_list())
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Forgets an added kubeconfig file (the file itself stays).
pub fn remove_kubeconfig_file(file: &std::path::Path) -> Result<()> {
    let keep: String = added_kubeconfig_files()
        .into_iter()
        .filter(|f| f != file)
        .map(|f| format!("{}\n", f.display()))
        .collect();
    Ok(std::fs::write(saved_list(), keep)?)
}

pub fn save_kubeconfig_files(added: &[PathBuf]) -> Result<()> {
    let path = saved_list();
    std::fs::create_dir_all(path.parent().unwrap())?;
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    for p in added {
        text.push_str(&format!("{}\n", p.display()));
    }
    Ok(std::fs::write(path, text)?)
}

pub fn load_contexts(files: &[PathBuf]) -> (Vec<Ctx>, Option<String>, Vec<String>) {
    let (mut ctxs, mut current, mut errs) = (vec![], None, vec![]);
    for file in files {
        match Kubeconfig::read_from(file) {
            Ok(kc) => {
                if current.is_none() {
                    current = kc.current_context.clone();
                }
                ctxs.extend(kc.contexts.into_iter().map(|c| Ctx {
                    name: c.name,
                    file: file.clone(),
                }));
            }
            Err(e) => errs.push(format!("{}: {e}", file.display())),
        }
    }
    (ctxs, current, errs)
}

async fn client(ctx: &Ctx) -> Result<Client> {
    let cell = CLIENTS.lock().unwrap().entry(ctx.clone()).or_default().clone();
    let c = cell
        .get_or_try_init(|| async {
            let kc = Kubeconfig::read_from(&ctx.file)?;
            let opts = KubeConfigOptions {
                context: Some(ctx.name.clone()),
                ..Default::default()
            };
            anyhow::Ok(Client::try_from(Config::from_custom_kubeconfig(kc, &opts).await?)?)
        })
        .await?;
    Ok(c.clone())
}

/// Only touches slots that still exist: `watch` creates its slot up front and `forget` removes it, so a
/// watcher that's been aborted but is still finishing an event can't resurrect a forgotten context.
fn update(key: &(Ctx, usize), f: impl FnOnce(&mut Slot)) {
    if let Some(slot) = CACHE.lock().unwrap().get_mut(key) {
        f(slot);
        VERSION.fetch_add(1, Ordering::Relaxed);
    }
}

fn row(ctx: &Ctx, kind: &Kind, mut o: DynamicObject) -> Arc<Row> {
    // Server-side field ownership: often a large share of each object, never shown. Not caching it
    // cuts memory a lot (YAML edits don't need it; the server keeps its own).
    o.metadata.managed_fields = None;
    let obj = serde_json::to_value(&o).unwrap_or_default();
    let created = o.metadata.creation_timestamp.map_or(0, |t| t.0.as_second());
    let created = match kind.kind {
        "Event" => ["/lastTimestamp", "/series/lastObservedTime", "/eventTime"]
            .iter()
            .find_map(|p| ts(&obj, p))
            .unwrap_or(created),
        _ => created,
    };
    Arc::new(Row {
        ctx: ctx.clone(),
        ns: o.metadata.namespace.unwrap_or_default(),
        name: o.metadata.name.unwrap_or_default(),
        cells: cells(kind, &obj),
        created,
        obj,
    })
}

/// Keeps one kind of one context mirrored in CACHE until the task is aborted.
pub fn watch(ctx: Ctx, ix: usize) -> tokio::task::JoinHandle<()> {
    CACHE.lock().unwrap().entry((ctx.clone(), ix)).or_default();
    RT.spawn(async move {
        let kind = self::kind(ix);
        let key = (ctx.clone(), ix);
        let client = loop {
            match client(&ctx).await {
                Ok(c) => break c,
                Err(e) => update(&key, |s| s.error = Some(format!("{e:#}"))),
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        };
        let mut stream = watcher(kind.api(client, ""), watcher::Config::default()).default_backoff().boxed();
        let mut init = BTreeMap::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(Event::Init) => init.clear(),
                Ok(Event::InitApply(o)) => {
                    let r = row(&ctx, kind, o);
                    init.insert((r.ns.clone(), r.name.clone()), r);
                }
                Ok(Event::InitDone) => update(&key, |s| {
                    s.rows = std::mem::take(&mut init);
                    s.synced = true;
                    s.error = None;
                }),
                Ok(Event::Apply(o)) => {
                    let r = row(&ctx, kind, o);
                    update(&key, |s| _ = s.rows.insert((r.ns.clone(), r.name.clone()), r));
                }
                Ok(Event::Delete(o)) => {
                    let k = (o.metadata.namespace.unwrap_or_default(), o.metadata.name.unwrap_or_default());
                    update(&key, |s| _ = s.rows.remove(&k));
                }
                Err(e) => update(&key, |s| s.error = Some(e.to_string())),
            }
        }
    })
}

/// Polls metrics-server for one context until aborted. Clusters without it simply have no metrics.
pub fn watch_metrics(ctx: Ctx) -> tokio::task::JoinHandle<()> {
    RT.spawn(async move {
        loop {
            let ok = match fetch_metrics(&ctx).await {
                Ok(m) => {
                    // Skip if the context was forgotten meanwhile (same reason as `update`).
                    if CACHE.lock().unwrap().keys().any(|(c, _)| *c == ctx) {
                        HISTORY.lock().unwrap().entry(ctx.clone()).or_default().record(&m);
                        METRICS.lock().unwrap().insert(ctx.clone(), Arc::new(m));
                    }
                    VERSION.fetch_add(1, Ordering::Relaxed);
                    true
                }
                Err(_) => false,
            };
            tokio::time::sleep(Duration::from_secs(if ok { 15 } else { 60 })).await;
        }
    })
}

async fn fetch_metrics(ctx: &Ctx) -> Result<Metrics> {
    let client = client(ctx).await?;
    let ar = |kind: &str, plural: &str| ApiResource {
        group: "metrics.k8s.io".into(),
        version: "v1beta1".into(),
        api_version: "metrics.k8s.io/v1beta1".into(),
        kind: kind.into(),
        plural: plural.into(),
    };
    let usage = |v: &Value| Usage {
        cpu: quantity(&s(v, "/cpu")).unwrap_or(0.),
        mem: quantity(&s(v, "/memory")).unwrap_or(0.),
    };
    let lp = Default::default();
    let pods = Api::<DynamicObject>::all_with(client.clone(), &ar("PodMetrics", "pods"))
        .list(&lp)
        .await?;
    let nodes = Api::<DynamicObject>::all_with(client, &ar("NodeMetrics", "nodes"))
        .list(&lp)
        .await?;
    let mut m = Metrics::default();
    for p in pods {
        let containers: Vec<(String, Usage)> = arr(&p.data, "/containers")
            .iter()
            .map(|c| (s(c, "/name"), usage(&c["usage"])))
            .collect();
        let mut total = Usage::default();
        containers.iter().for_each(|(_, u)| total += *u);
        let key = (p.metadata.namespace.unwrap_or_default(), p.metadata.name.unwrap_or_default());
        m.pods.insert(key, (total, containers));
    }
    for n in nodes {
        m.nodes.insert(n.metadata.name.unwrap_or_default(), usage(&n.data["usage"]));
    }
    Ok(m)
}

/// Parses a Kubernetes quantity ("250m", "1.5", "128Mi", "137131461n") into base units (cores / bytes).
pub fn quantity(q: &str) -> Option<f64> {
    const SUFFIXES: [(&str, f64); 15] = [
        ("Ki", 1024.),
        ("Mi", 1048576.),
        ("Gi", 1073741824.),
        ("Ti", 1099511627776.),
        ("Pi", 1125899906842624.),
        ("Ei", 1152921504606846976.),
        ("n", 1e-9),
        ("u", 1e-6),
        ("m", 1e-3),
        ("k", 1e3),
        ("M", 1e6),
        ("G", 1e9),
        ("T", 1e12),
        ("P", 1e15),
        ("E", 1e18),
    ];
    let q = q.trim();
    let (num, mult) = SUFFIXES
        .iter()
        .find_map(|(suf, mult)| Some((q.strip_suffix(suf)?, *mult)))
        .unwrap_or((q, 1.));
    if num.is_empty() || !num.starts_with(|c: char| c.is_ascii_digit() || c == '.' || c == '-') {
        return None;
    }
    num.parse::<f64>().ok().map(|n| n * mult)
}

/// Summed container (requests, limits) of a pod spec.
pub fn pod_resources(pod: &Value) -> (Usage, Usage) {
    let (mut req, mut lim) = (Usage::default(), Usage::default());
    for c in arr(pod, "/spec/containers") {
        let (r, l) = container_resources(c);
        req += r;
        lim += l;
    }
    (req, lim)
}

pub fn container_resources(c: &Value) -> (Usage, Usage) {
    let get = |kind: &str| Usage {
        cpu: quantity(&s(c, &format!("/resources/{kind}/cpu"))).unwrap_or(0.),
        mem: quantity(&s(c, &format!("/resources/{kind}/memory"))).unwrap_or(0.),
    };
    (get("requests"), get("limits"))
}

/// Seconds since epoch of an RFC3339 timestamp field.
pub fn ts(v: &Value, path: &str) -> Option<i64> {
    s(v, path).parse::<Timestamp>().ok().map(|t| t.as_second())
}

/// A running port-forward: listens on 127.0.0.1:`local` and tunnels each connection to the pod.
pub struct Forward {
    pub local: u16,
    pub active: Arc<std::sync::atomic::AtomicUsize>,
    pub error: Arc<Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Forward {
    /// Stopping = dropping: the listener task owns the connection tasks, so they end with it.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Binds 127.0.0.1 on the first usable port: `want`, then `want + 10000`, then any free port.
/// `exact` (a port the user typed) means only `want`, with an error if it's taken.
async fn bind_local(want: u16, exact: bool) -> Result<tokio::net::TcpListener> {
    let mut candidates = vec![want];
    if !exact {
        candidates.extend(want.checked_add(10000));
        candidates.push(0);
    }
    // macOS lets us bind 127.0.0.1:N even when another process listens on *:N, which would silently
    // hijack that process's localhost traffic. So treat any port that already answers (v4 or v6) as taken.
    let answers = |host: &'static str, port: u16| async move {
        matches!(
            tokio::time::timeout(Duration::from_millis(300), tokio::net::TcpStream::connect((host, port))).await,
            Ok(Ok(_))
        )
    };
    for port in candidates {
        if port != 0 && (answers("127.0.0.1", port).await || answers("::1", port).await) {
            continue;
        }
        if let Ok(l) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return Ok(l);
        }
    }
    Err(if exact {
        anyhow::anyhow!("localhost:{want} is already in use")
    } else {
        anyhow::anyhow!("no free local port")
    })
}

/// A local port that's free right now (for prefilling the port field).
pub async fn suggest_port(want: u16) -> Result<u16> {
    Ok(bind_local(want, false).await?.local_addr()?.port())
}

/// Starts forwarding `local_port` (exact) or the best free port near `want` to the pod's `remote` port.
pub async fn port_forward(ctx: Ctx, ns: String, pod: String, remote: u16, want: u16, exact: bool) -> Result<Forward> {
    let api: Api<Pod> = Api::namespaced(client(&ctx).await?, &ns);
    let listener = bind_local(want, exact).await?;
    let local = listener.local_addr()?.port();
    let (active, error) = (Arc::new(std::sync::atomic::AtomicUsize::new(0)), Arc::new(Mutex::new(None)));
    let task = RT.spawn({
        let (active, error) = (active.clone(), error.clone());
        async move {
            let mut conns = tokio::task::JoinSet::new();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { continue };
                while conns.try_join_next().is_some() {}
                let (api, pod, active, error) = (api.clone(), pod.clone(), active.clone(), error.clone());
                conns.spawn(async move {
                    active.fetch_add(1, Ordering::Relaxed);
                    let res: Result<()> = async {
                        let mut pf = api.portforward(&pod, &[remote]).await?;
                        let mut upstream = pf
                            .take_stream(remote)
                            .ok_or_else(|| anyhow::anyhow!("no stream for port {remote}"))?;
                        tokio::io::copy_bidirectional(&mut sock, &mut upstream).await.ok();
                        drop(upstream);
                        pf.join().await.ok();
                        Ok(())
                    }
                    .await;
                    *error.lock().unwrap() = res.err().map(|e| format!("{e:#}"));
                    active.fetch_sub(1, Ordering::Relaxed);
                    VERSION.fetch_add(1, Ordering::Relaxed);
                });
                VERSION.fetch_add(1, Ordering::Relaxed);
            }
        }
    });
    Ok(Forward {
        local,
        active,
        error,
        task,
    })
}

/// Drops cached data and client for a context (after its watchers are aborted).
pub fn forget(ctx: &Ctx) {
    CACHE.lock().unwrap().retain(|(c, _), _| c != ctx);
    METRICS.lock().unwrap().remove(ctx);
    HISTORY.lock().unwrap().remove(ctx);
    CLIENTS.lock().unwrap().remove(ctx);
    VERSION.fetch_add(1, Ordering::Relaxed);
}

pub struct LogLine {
    /// Index into `LogStream::pods`.
    pub pod: usize,
    pub text: String,
    pub level: Level,
}

/// Log severity, least to most severe (`None`: the line doesn't say).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Level {
    #[default]
    None,
    Debug,
    Info,
    Warn,
    Error,
}

/// Severity of a log line: JSON/logfmt `level`/`severity` (words or pino numbers), klog's
/// `E1011 …` prefix, or a bare ERROR/WARN/INFO/DEBUG word near the start. `Level::None` when there's none.
pub fn log_level(line: &str) -> Level {
    level_of(line).unwrap_or(Level::None)
}

fn level_of(line: &str) -> Option<Level> {
    let word = |w: &str| -> Option<Level> {
        let w = w
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let pick = |p: &[&str]| p.contains(&w.as_str());
        Some(
            if pick(&["err", "error", "fatal", "panic", "crit", "critical", "emerg", "emergency", "alert"]) {
                Level::Error
            } else if pick(&["warn", "warning"]) {
                Level::Warn
            } else if pick(&["info", "notice"]) {
                Level::Info
            } else if pick(&["debug", "trace", "verbose"]) {
                Level::Debug
            } else {
                return None;
            },
        )
    };
    let head = &line[..line.floor_char_boundary(300)];
    let lower = head.to_ascii_lowercase();
    for key in ["\"level\"", "\"severity\"", "\"lvl\"", "level=", "severity=", "lvl="] {
        if let Some(at) = lower.find(key) {
            let v = head[at + key.len()..].trim_start_matches([':', ' ', '"']);
            let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(n) = digits.parse::<u32>() {
                // pino/bunyan: 10 trace, 20 debug, 30 info, 40 warn, 50 error, 60 fatal
                return Some(match n {
                    ..30 => Level::Debug,
                    30..40 => Level::Info,
                    40..50 => Level::Warn,
                    _ => Level::Error,
                });
            }
            if let Some(c) = word(v) {
                return Some(c);
            }
        }
    }
    let b = head.as_bytes();
    if b.len() > 5 && b"IWEF".contains(&b[0]) && b[1..5].iter().all(u8::is_ascii_digit) {
        return word(match b[0] {
            b'I' => "info",
            b'W' => "warn",
            _ => "error",
        });
    }
    // `fatal: …`, `error: …` as the first word, any case.
    let t = head.trim_start();
    let first = t.split(|c: char| !c.is_ascii_alphabetic()).next().unwrap_or("");
    if !first.is_empty()
        && t[first.len()..].starts_with(':')
        && let Some(c) = word(first)
    {
        return Some(c);
    }
    head.split(|c: char| !c.is_ascii_alphabetic())
        .filter(|w| !w.is_empty())
        .take(12)
        .find(|w| {
            matches!(
                *w,
                "ERROR" | "ERR" | "FATAL" | "PANIC" | "WARN" | "WARNING" | "INFO" | "DEBUG" | "TRACE"
            )
        })
        .and_then(word)
}

/// Live log lines of one container across one or more pods (newest last, capped).
/// Stopping = dropping it.
pub struct LogStream {
    pub pods: Vec<String>,
    pub lines: Arc<Mutex<std::collections::VecDeque<LogLine>>>,
    /// Bumped on every new line so the UI knows when to redraw.
    pub version: Arc<AtomicU64>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for LogStream {
    fn drop(&mut self) {
        self.tasks.iter().for_each(|t| t.abort());
    }
}

const MAX_LOG_LINES: usize = 20_000;

/// Follows logs (`kubectl logs -f`) for each pod; `previous` shows the last crashed container instead.
/// `since`: only lines from the last that many seconds (else the last ~2000 lines).
pub fn stream_logs(
    ctx: Ctx,
    ns: String,
    pods: Vec<String>,
    container: Option<String>,
    previous: bool,
    timestamps: bool,
    since: Option<i64>,
) -> LogStream {
    let lines: Arc<Mutex<std::collections::VecDeque<LogLine>>> = Default::default();
    let version = Arc::new(AtomicU64::new(0));
    // Spread the initial history over the pods.
    let tail = (2000 / pods.len().max(1)).max(200) as i64;
    let tasks = pods
        .iter()
        .enumerate()
        .map(|(i, pod)| {
            let (ctx, ns, pod, container) = (ctx.clone(), ns.clone(), pod.clone(), container.clone());
            let (lines, version) = (lines.clone(), version.clone());
            RT.spawn(async move {
                let push = |text: String| {
                    let mut l = lines.lock().unwrap();
                    if l.len() >= MAX_LOG_LINES {
                        l.pop_front();
                    }
                    let level = log_level(&text);
                    l.push_back(LogLine { pod: i, text, level });
                    version.fetch_add(1, Ordering::Relaxed);
                };
                let res: Result<()> = async {
                    use futures::AsyncBufReadExt;
                    let api: Api<Pod> = Api::namespaced(client(&ctx).await?, &ns);
                    let lp = LogParams {
                        container,
                        follow: !previous,
                        previous,
                        timestamps,
                        tail_lines: if since.is_some() { None } else { Some(tail) },
                        since_seconds: since,
                        ..Default::default()
                    };
                    let mut stream = api.log_stream(&pod, &lp).await?.lines();
                    while let Some(line) = stream.next().await {
                        push(line?);
                    }
                    Ok(())
                }
                .await;
                match res {
                    Err(e) => push(format!("⚠ {e:#}")),
                    Ok(()) if !previous => push("— log stream ended (container stopped?) —".into()),
                    Ok(()) => {}
                }
            })
        })
        .collect();
    LogStream {
        pods,
        lines,
        version,
        tasks,
    }
}

/// `dry_run` asks the API server to fully validate without persisting anything.
pub async fn delete(ctx: Ctx, kind: &'static Kind, ns: String, name: String, dry_run: bool) -> Result<()> {
    // Background propagation like kubectl; the API default orphans a Job's pods.
    let dp = DeleteParams {
        dry_run,
        ..DeleteParams::background()
    };
    kind.api(client(&ctx).await?, &ns).delete(&name, &dp).await?;
    Ok(())
}

/// Same as `kubectl rollout restart`.
pub async fn restart(ctx: Ctx, kind: &'static Kind, ns: String, name: String, dry_run: bool) -> Result<()> {
    let api = kind.api(client(&ctx).await?, &ns);
    let patch = json!({"spec": {"template": {"metadata": {"annotations": {
        "kubectl.kubernetes.io/restartedAt": Timestamp::now().to_string()
    }}}}});
    api.patch(
        &name,
        &PatchParams {
            dry_run,
            ..Default::default()
        },
        &Patch::Merge(&patch),
    )
    .await?;
    Ok(())
}

/// Merge-patches an object (used for Flux reconcile / suspend / resume).
pub async fn patch(ctx: Ctx, kind: &'static Kind, ns: String, name: String, patch: Value, dry_run: bool) -> Result<()> {
    let pp = PatchParams {
        dry_run,
        ..Default::default()
    };
    kind.api(client(&ctx).await?, &ns).patch(&name, &pp, &Patch::Merge(&patch)).await?;
    Ok(())
}

/// Like `kubectl drain --ignore-daemonsets --delete-emptydir-data`: cordon, then evict the node's pods
/// through the Eviction API, so PodDisruptionBudgets are respected (a blocked eviction is retried for up
/// to 5 minutes). Running pods without a controller would be gone for good, so they're left on the node
/// (kubectl wants --force for those). Doesn't wait for evicted pods to finish terminating.
pub async fn drain(ctx: Ctx, node: String) -> Result<String> {
    let c = client(&ctx).await?;
    let unschedulable = json!({"spec": {"unschedulable": true}});
    Api::<k8s_openapi::api::core::v1::Node>::all(c.clone())
        .patch(&node, &PatchParams::default(), &Patch::Merge(&unschedulable))
        .await?;
    let pods = Api::<Pod>::all(c.clone())
        .list(&ListParams::default().fields(&format!("spec.nodeName={node}")))
        .await?;
    let (mut todo, mut kept) = (vec![], vec![]);
    for pod in pods {
        let Some(evict) = drain_action(&pod) else { continue };
        let (ns, name) = (pod.metadata.namespace.unwrap_or_default(), pod.metadata.name.unwrap_or_default());
        if evict {
            todo.push((ns, name))
        } else {
            kept.push(format!("{ns}/{name}"))
        }
    }
    // Evict everything; pods a disruption budget protects right now (429) are retried every 5 seconds
    // until their replacements are up elsewhere, for at most 5 minutes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
    let (total, mut blocked) = (todo.len(), String::new());
    loop {
        let mut retry = vec![];
        for (ns, name) in todo {
            match Api::<Pod>::namespaced(c.clone(), &ns).evict(&name, &EvictParams::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(e)) if e.code == 404 => {}
                Err(kube::Error::Api(e)) if e.code == 429 => {
                    let cause = e.details.iter().flat_map(|d| &d.causes).next().map(|c| c.message.clone());
                    blocked = cause.unwrap_or(e.message);
                    retry.push((ns, name));
                }
                Err(e) => anyhow::bail!("evicting {ns}/{name}: {e} (node stays cordoned)"),
            }
        }
        todo = retry;
        if todo.is_empty() || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    if !todo.is_empty() {
        let pods: Vec<String> = todo.iter().map(|(ns, name)| format!("{ns}/{name}")).collect();
        anyhow::bail!("gave up after 5 minutes on {}: {blocked} (node stays cordoned)", pods.join(", "));
    }
    let evicted = total;
    let mut msg = format!("{evicted} pod{} evicted", if evicted == 1 { "" } else { "s" });
    if !kept.is_empty() {
        msg += &format!("; kept {} without a controller", kept.join(", "));
    }
    Ok(msg)
}

/// What draining does with a pod: None = leave it (DaemonSet or static pod), Some(true) = evict,
/// Some(false) = keep, because nothing would recreate it.
fn drain_action(pod: &Pod) -> Option<bool> {
    let owner = pod.metadata.owner_references.iter().flatten().find(|o| o.controller == Some(true));
    let mirror = pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key("kubernetes.io/config.mirror"));
    if mirror || owner.is_some_and(|o| o.kind == "DaemonSet") {
        return None;
    }
    let done = matches!(pod.status.as_ref().and_then(|s| s.phase.as_deref()), Some("Succeeded" | "Failed"));
    Some(owner.is_some() || done)
}

/// Same as `flux reconcile`: Flux controllers act on a changed requestedAt annotation.
pub fn reconcile_patch() -> Value {
    json!({"metadata": {"annotations": {"reconcile.fluxcd.io/requestedAt": Timestamp::now().to_string()}}})
}

/// Registers every CRD served by a context; returns their kind indices.
pub async fn discover(ctx: Ctx) -> Result<Vec<usize>> {
    let crds = Api::<CustomResourceDefinition>::all(client(&ctx).await?)
        .list(&Default::default())
        .await?;
    Ok(crds.items.iter().filter_map(register_crd).collect())
}

fn register_crd(crd: &CustomResourceDefinition) -> Option<usize> {
    let spec = &crd.spec;
    let version = spec
        .versions
        .iter()
        .find(|v| v.storage)
        .or_else(|| spec.versions.iter().find(|v| v.served))?;
    if let Some(ix) = find_kind(|k| k.group == spec.group && k.plural == spec.names.plural) {
        return Some(ix);
    }
    let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
    // "GitRepository" + "gitrepositories" -> "GitRepositories": keep the kind's casing for the shared prefix.
    let (kind, plural) = (&spec.names.kind, &spec.names.plural);
    let same = kind.to_lowercase().chars().zip(plural.chars()).take_while(|(a, b)| a == b).count();
    let label = format!(
        "{}{}",
        &kind[..kind.char_indices().nth(same).map_or(kind.len(), |(i, _)| i)],
        &plural[same.min(plural.len())..]
    );
    let printer: Vec<(&'static str, &'static str)> = version
        .additional_printer_columns
        .iter()
        .flatten()
        .filter(|c| c.priority.unwrap_or(0) == 0 && c.type_ != "date" && c.name != "Age")
        .map(|c| {
            // Long Ready messages get the flexible "Message" column.
            let name = if c.json_path.ends_with(".message") {
                "Message".to_string()
            } else {
                c.name.clone()
            };
            (leak(name), leak(c.json_path.clone()))
        })
        .collect();
    let icon = match kind.as_str() {
        "Kustomization" => "file-stack",
        "HelmRelease" => "ship",
        "GitRepository" => "git-branch",
        "OCIRepository" => "package",
        "HelmRepository" => "anchor",
        "HelmChart" | "Bucket" => "archive",
        "Alert" => "bell",
        "Provider" => "send",
        "Receiver" => "radio-receiver",
        _ => "puzzle",
    };
    let k = Kind {
        name: leak(label),
        icon,
        group: leak(spec.group.clone()),
        version: leak(version.name.clone()),
        kind: leak(kind.clone()),
        plural: leak(plural.clone()),
        namespaced: spec.scope == "Namespaced",
        cols: Box::leak(printer.iter().map(|(c, _)| *c).collect::<Vec<_>>().into_boxed_slice()),
        printer: Box::leak(printer.into_boxed_slice()),
        custom: true,
        lazy: false,
    };
    let mut reg = REGISTRY.write().unwrap();
    reg.push(Box::leak(Box::new(k)));
    Some(reg.len() - 1)
}

/// The JSONPath subset CRD printer columns use: `.a.b`, `[0]`, `[*]`, `[?(@.type=="Ready")]`.
pub fn jsonpath(v: &Value, path: &str) -> String {
    let mut cur = vec![v];
    let mut rest = path.trim().trim_start_matches('{').trim_end_matches('}').trim_start_matches('$');
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('.') {
            rest = r;
        } else if let Some(r) = rest.strip_prefix('[') {
            let Some(end) = r.find(']') else { break };
            let (inner, tail) = (r[..end].trim(), &r[end + 1..]);
            rest = tail;
            let items = cur.iter().flat_map(|v| v.as_array().into_iter().flatten());
            cur = if inner == "*" {
                items.collect()
            } else if let Ok(i) = inner.parse::<usize>() {
                cur.iter().filter_map(|v| v.get(i)).collect()
            } else if let Some(filter) = inner.strip_prefix("?(").and_then(|f| f.strip_suffix(')')) {
                let Some((lhs, rhs)) = filter.split_once("==") else {
                    return String::new();
                };
                let (lhs, rhs) = (
                    lhs.trim().trim_start_matches('@'),
                    rhs.trim().trim_matches(|c| c == '"' || c == '\''),
                );
                items.filter(|item| jsonpath(item, lhs) == rhs).collect()
            } else {
                vec![]
            };
        } else {
            let end = rest.find(['.', '[']).unwrap_or(rest.len());
            let key = &rest[..end];
            rest = &rest[end..];
            cur = cur.iter().filter_map(|v| v.get(key)).collect();
        }
    }
    let text: Vec<String> = cur
        .iter()
        .map(|v| match v {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            other => other.to_string(),
        })
        .collect();
    text.join(",")
}

/// Same as `kubectl create job --from=cronjob/<name>`. Returns the new Job's name.
pub async fn trigger(ctx: Ctx, ns: String, name: String, dry_run: bool) -> Result<String> {
    let client = client(&ctx).await?;
    let kind = |k: &str| BUILTIN.iter().find(|x| x.kind == k).expect("known kind");
    let cj = kind("CronJob").api(client.clone(), &ns).get(&name).await?;
    let tmpl = &cj.data["spec"]["jobTemplate"];
    // Job names feed the 63-char `job-name` label.
    let base = name[..name.len().min(44)].trim_end_matches('-');
    let job_name = format!("{base}-manual-{}", Timestamp::now().as_second() % 1_000_000);
    let job: DynamicObject = serde_json::from_value(json!({
        "apiVersion": "batch/v1",
        "kind": "Job",
        "metadata": {
            "name": job_name,
            "namespace": ns,
            "labels": tmpl["metadata"]["labels"],
            "annotations": {"cronjob.kubernetes.io/instantiate": "manual"},
            "ownerReferences": [{
                "apiVersion": "batch/v1",
                "kind": "CronJob",
                "name": name,
                "uid": cj.metadata.uid,
                "controller": true,
                "blockOwnerDeletion": true,
            }],
        },
        "spec": tmpl["spec"],
    }))?;
    kind("Job")
        .api(client, &ns)
        .create(
            &PostParams {
                dry_run,
                ..Default::default()
            },
            &job,
        )
        .await?;
    Ok(job_name)
}

/// Replaces an object with edited YAML (PUT). The YAML's resourceVersion makes concurrent edits fail
/// with a conflict instead of silently overwriting.
pub async fn replace(ctx: Ctx, kind: &'static Kind, ns: String, name: String, yaml: String, dry_run: bool) -> Result<()> {
    let obj: DynamicObject = serde_yaml::from_str(&yaml)?;
    let pp = PostParams {
        dry_run,
        ..Default::default()
    };
    kind.api(client(&ctx).await?, &ns).replace(&name, &pp, &obj).await?;
    Ok(())
}

pub fn age(created: i64) -> String {
    let s = Timestamp::now().as_second() - created;
    match s {
        _ if created == 0 => "-".into(),
        ..120 => format!("{s}s"),
        120..7200 => format!("{}m", s / 60),
        7200..172800 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

pub fn s(v: &Value, path: &str) -> String {
    match v.pointer(path) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(x) => x.to_string(),
    }
}

fn n(v: &Value, path: &str) -> i64 {
    v.pointer(path).and_then(Value::as_i64).unwrap_or(0)
}

pub fn arr<'a>(v: &'a Value, path: &str) -> &'a [Value] {
    v.pointer(path).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

fn len(v: &Value, path: &str) -> String {
    v.pointer(path).and_then(Value::as_object).map_or(0, |m| m.len()).to_string()
}

/// A scalar as text (strings unquoted).
fn plain(v: &Value) -> String {
    s(v, "")
}

/// A label selector's matchLabels as `k=v,…` (`<all>` when it selects everything).
fn selector(v: Option<&Value>) -> String {
    let labels: Vec<String> = v
        .and_then(|v| v.pointer("/matchLabels"))
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(k, v)| format!("{k}={}", plain(v)))
        .collect();
    let exprs = v.map_or(0, |v| arr(v, "/matchExpressions").len());
    match (labels.is_empty(), exprs) {
        (true, 0) => "<all>".into(),
        (_, 0) => labels.join(","),
        _ => format!("{}{}+{exprs} expr", labels.join(","), if labels.is_empty() { "" } else { " " }),
    }
}

fn cells(kind: &Kind, v: &Value) -> Vec<String> {
    if kind.custom {
        // `kubectl get` columns; Flux-style suspended objects say so instead of a stale Ready.
        let suspended = v.pointer("/spec/suspend") == Some(&Value::Bool(true));
        return kind
            .printer
            .iter()
            .map(|(col, path)| {
                if suspended && *col == "Ready" {
                    "Suspended".into()
                } else {
                    jsonpath(v, path)
                }
            })
            .collect();
    }
    match kind.kind {
        "Pod" => {
            let cs = arr(v, "/status/containerStatuses");
            let ready = cs.iter().filter(|c| c["ready"] == true).count();
            let reason = cs.iter().find_map(|c| {
                c.pointer("/state/waiting/reason")
                    .or(c.pointer("/state/terminated/reason"))?
                    .as_str()
            });
            let status = if v.pointer("/metadata/deletionTimestamp").is_some() {
                "Terminating".into()
            } else {
                reason.map_or_else(|| s(v, "/status/phase"), str::to_string)
            };
            let restarts: i64 = cs.iter().map(|c| c["restartCount"].as_i64().unwrap_or(0)).sum();
            let total = arr(v, "/spec/containers").len();
            vec![format!("{ready}/{total}"), status, restarts.to_string(), s(v, "/spec/nodeName")]
        }
        "Deployment" => vec![
            format!("{}/{}", n(v, "/status/readyReplicas"), n(v, "/spec/replicas")),
            n(v, "/status/updatedReplicas").to_string(),
            n(v, "/status/availableReplicas").to_string(),
        ],
        "StatefulSet" => vec![format!("{}/{}", n(v, "/status/readyReplicas"), n(v, "/spec/replicas"))],
        "DaemonSet" => vec![
            n(v, "/status/desiredNumberScheduled").to_string(),
            n(v, "/status/numberReady").to_string(),
        ],
        "Job" => {
            let status = arr(v, "/status/conditions")
                .iter()
                .find(|c| c["status"] == "True" && matches!(c["type"].as_str(), Some("Complete" | "Failed" | "Suspended")))
                .map_or("Running".into(), |c| s(c, "/type"));
            vec![format!("{}/{}", n(v, "/status/succeeded"), n(v, "/spec/completions")), status]
        }
        "CronJob" => vec![
            s(v, "/spec/schedule"),
            s(v, "/spec/suspend"),
            arr(v, "/status/active").len().to_string(),
        ],
        "Service" => {
            let ports: Vec<String> = arr(v, "/spec/ports")
                .iter()
                .map(|p| format!("{}/{}", p["port"], p["protocol"].as_str().unwrap_or("TCP")))
                .collect();
            vec![s(v, "/spec/type"), s(v, "/spec/clusterIP"), ports.join(",")]
        }
        "Ingress" => {
            let hosts: Vec<String> = arr(v, "/spec/rules").iter().map(|r| s(r, "/host")).collect();
            vec![s(v, "/spec/ingressClassName"), hosts.join(",")]
        }
        "ConfigMap" => vec![len(v, "/data")],
        "Secret" => vec![s(v, "/type"), len(v, "/data")],
        "PersistentVolumeClaim" => {
            vec![
                s(v, "/status/phase"),
                s(v, "/status/capacity/storage"),
                s(v, "/spec/storageClassName"),
            ]
        }
        "Event" => vec![
            s(v, "/type"),
            s(v, "/reason"),
            format!("{}/{}", s(v, "/involvedObject/kind"), s(v, "/involvedObject/name")),
            s(v, "/message"),
        ],
        "Node" => {
            let ready = arr(v, "/status/conditions")
                .iter()
                .any(|c| c["type"] == "Ready" && c["status"] == "True");
            let roles: Vec<&str> = v
                .pointer("/metadata/labels")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|m| m.keys())
                .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
                .collect();
            let cordoned = v.pointer("/spec/unschedulable") == Some(&Value::Bool(true));
            let status = match (ready, cordoned) {
                (false, _) => "NotReady",
                (true, true) => "Cordoned",
                (true, false) => "Ready",
            };
            vec![status.into(), roles.join(",")]
        }
        "Namespace" => vec![s(v, "/status/phase")],
        "ReplicaSet" => vec![
            n(v, "/spec/replicas").to_string(),
            n(v, "/status/replicas").to_string(),
            n(v, "/status/readyReplicas").to_string(),
        ],
        "NetworkPolicy" => {
            let types: Vec<String> = arr(v, "/spec/policyTypes")
                .iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect();
            vec![selector(v.pointer("/spec/podSelector")), types.join(",")]
        }
        "HorizontalPodAutoscaler" => {
            // "cpu 45%/80%" per resource metric, like `kubectl get hpa`.
            let current = arr(v, "/status/currentMetrics");
            let metrics: Vec<String> = arr(v, "/spec/metrics")
                .iter()
                .filter_map(|m| {
                    let name = m.pointer("/resource/name")?.as_str()?;
                    let target = m
                        .pointer("/resource/target/averageUtilization")
                        .map(|t| format!("{t}%"))
                        .or_else(|| m.pointer("/resource/target/averageValue").and_then(Value::as_str).map(String::from))?;
                    let now = current
                        .iter()
                        .find(|c| c.pointer("/resource/name").and_then(Value::as_str) == Some(name))
                        .and_then(|c| {
                            c.pointer("/resource/current/averageUtilization")
                                .map(|u| format!("{u}%"))
                                .or_else(|| {
                                    c.pointer("/resource/current/averageValue")
                                        .and_then(Value::as_str)
                                        .map(String::from)
                                })
                        })
                        .unwrap_or("?".into());
                    Some(format!("{name} {now}/{target}"))
                })
                .collect();
            vec![
                format!("{}/{}", s(v, "/spec/scaleTargetRef/kind"), s(v, "/spec/scaleTargetRef/name")),
                metrics.join(", "),
                v.pointer("/spec/minReplicas").map_or("1".into(), |m| m.to_string()),
                n(v, "/spec/maxReplicas").to_string(),
                n(v, "/status/currentReplicas").to_string(),
            ]
        }
        "PodDisruptionBudget" => vec![
            v.pointer("/spec/minAvailable").map(plain).unwrap_or_default(),
            v.pointer("/spec/maxUnavailable").map(plain).unwrap_or_default(),
            n(v, "/status/disruptionsAllowed").to_string(),
        ],
        "ResourceQuota" => {
            let used = v.pointer("/status/used");
            let hard: Vec<String> = v
                .pointer("/status/hard")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(k, h)| format!("{k} {}/{}", used.and_then(|u| u.get(k)).map(plain).unwrap_or("0".into()), plain(h)))
                .collect();
            vec![hard.join(", ")]
        }
        "LimitRange" => {
            let types: Vec<String> = arr(v, "/spec/limits").iter().map(|l| s(l, "/type")).collect();
            vec![types.join(",")]
        }
        "PersistentVolume" => {
            let claim = match s(v, "/spec/claimRef/name") {
                c if c.is_empty() => c,
                c => format!("{}/{c}", s(v, "/spec/claimRef/namespace")),
            };
            vec![
                s(v, "/status/phase"),
                s(v, "/spec/capacity/storage"),
                claim,
                s(v, "/spec/storageClassName"),
                s(v, "/spec/persistentVolumeReclaimPolicy"),
            ]
        }
        "StorageClass" => {
            let default = v.pointer("/metadata/annotations/storageclass.kubernetes.io~1is-default-class") == Some(&json!("true"));
            vec![
                s(v, "/provisioner"),
                s(v, "/reclaimPolicy"),
                s(v, "/volumeBindingMode"),
                if default { "Yes".into() } else { String::new() },
            ]
        }
        "ServiceAccount" => vec![arr(v, "/secrets").len().to_string()],
        "Role" | "ClusterRole" => vec![arr(v, "/rules").len().to_string()],
        "RoleBinding" | "ClusterRoleBinding" => {
            let subjects: Vec<String> = arr(v, "/subjects")
                .iter()
                .map(|s| format!("{}/{}", s["kind"].as_str().unwrap_or(""), s["name"].as_str().unwrap_or("")))
                .collect();
            vec![format!("{}/{}", s(v, "/roleRef/kind"), s(v, "/roleRef/name")), subjects.join(", ")]
        }
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drain_picks() {
        let pod = |owner: &str, phase: &str, mirror: bool| -> Pod {
            serde_json::from_value(json!({
                "metadata": {
                    "name": "p",
                    "ownerReferences": if owner.is_empty() { json!([]) } else {
                        json!([{"apiVersion": "apps/v1", "kind": owner, "name": "o", "uid": "u", "controller": true}])
                    },
                    "annotations": if mirror { json!({"kubernetes.io/config.mirror": "x"}) } else { json!({}) },
                },
                "status": {"phase": phase},
            }))
            .unwrap()
        };
        assert_eq!(drain_action(&pod("ReplicaSet", "Running", false)), Some(true));
        assert_eq!(drain_action(&pod("DaemonSet", "Running", false)), None);
        assert_eq!(drain_action(&pod("", "Running", true)), None);
        assert_eq!(drain_action(&pod("", "Running", false)), Some(false));
        assert_eq!(drain_action(&pod("", "Succeeded", false)), Some(true));
    }

    #[test]
    fn pod_cells() {
        let v = json!({
            "spec": {"containers": [{}, {}], "nodeName": "n1"},
            "status": {"phase": "Running", "containerStatuses": [
                {"ready": true, "restartCount": 2, "state": {"running": {}}},
                {"ready": false, "restartCount": 3, "state": {"waiting": {"reason": "CrashLoopBackOff"}}},
            ]}
        });
        let builtin = |k: &str| BUILTIN.iter().find(|b| b.kind == k).unwrap();
        assert_eq!(cells(builtin("Pod"), &v), ["1/2", "CrashLoopBackOff", "5", "n1"]);
        assert_eq!(
            cells(builtin("Service"), &json!({"spec": {"type": "ClusterIP", "ports": [{"port": 80}]}}))[2],
            "80/TCP"
        );
    }

    #[test]
    fn log_levels() {
        assert_eq!(log_level(r#"{"level":30,"msg":"connected"}"#), Level::Info);
        assert_eq!(log_level(r#"{"level":"error","msg":"x"}"#), Level::Error);
        assert_eq!(log_level(r#"{"severity": "WARNING"}"#), Level::Warn);
        assert_eq!(log_level("ts=1 level=debug msg=hi"), Level::Debug);
        assert_eq!(log_level("E1011 12:00:00.000 1 reflector.go:1] boom"), Level::Error);
        assert_eq!(log_level("2026-10-11 12:00:00 WARN disk almost full"), Level::Warn);
        assert_eq!(log_level("GET /healthz 200"), Level::None);
        assert_eq!(log_level("fatal: PAYMENTS_TOKEN not set"), Level::Error);
        assert_eq!(log_level("information: none"), Level::None);
        assert_eq!(log_level("Information about errors"), Level::None);
    }

    #[test]
    fn more_builtin_cells() {
        let builtin = |k: &str| BUILTIN.iter().find(|b| b.kind == k).unwrap();
        let hpa = json!({
            "spec": {"scaleTargetRef": {"kind": "Deployment", "name": "api"}, "minReplicas": 2, "maxReplicas": 6,
                "metrics": [{"type": "Resource", "resource": {"name": "cpu", "target": {"type": "Utilization", "averageUtilization": 80}}}]},
            "status": {"currentReplicas": 3, "currentMetrics": [{"type": "Resource", "resource": {"name": "cpu", "current": {"averageUtilization": 45}}}]}
        });
        assert_eq!(
            cells(builtin("HorizontalPodAutoscaler"), &hpa),
            ["Deployment/api", "cpu 45%/80%", "2", "6", "3"]
        );
        let pdb = json!({"spec": {"minAvailable": "50%"}, "status": {"disruptionsAllowed": 1}});
        assert_eq!(cells(builtin("PodDisruptionBudget"), &pdb), ["50%", "", "1"]);
        let pv = json!({"spec": {"capacity": {"storage": "2Gi"}, "claimRef": {"namespace": "shop", "name": "data"},
            "storageClassName": "standard", "persistentVolumeReclaimPolicy": "Delete"}, "status": {"phase": "Bound"}});
        assert_eq!(
            cells(builtin("PersistentVolume"), &pv),
            ["Bound", "2Gi", "shop/data", "standard", "Delete"]
        );
        let rb = json!({"roleRef": {"kind": "ClusterRole", "name": "view"}, "subjects": [{"kind": "ServiceAccount", "name": "ci"}]});
        assert_eq!(cells(builtin("RoleBinding"), &rb), ["ClusterRole/view", "ServiceAccount/ci"]);
        let np = json!({"spec": {"podSelector": {}, "policyTypes": ["Ingress"]}});
        assert_eq!(cells(builtin("NetworkPolicy"), &np), ["<all>", "Ingress"]);
        let quota = json!({"status": {"hard": {"pods": "10"}, "used": {"pods": "3"}}});
        assert_eq!(cells(builtin("ResourceQuota"), &quota), ["pods 3/10"]);
    }

    #[test]
    fn local_port_choice() {
        RT.block_on(async {
            // Something already listening: a typed (exact) port must fail, auto-pick must move elsewhere.
            let taken = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let busy = taken.local_addr().unwrap().port();
            let err = bind_local(busy, true).await.unwrap_err().to_string();
            assert!(err.contains("already in use"), "{err}");
            let other = bind_local(busy, false).await.unwrap().local_addr().unwrap().port();
            assert_ne!(other, busy);
            // A free port is used exactly as typed.
            let free = suggest_port(0).await.unwrap();
            assert_eq!(bind_local(free, true).await.unwrap().local_addr().unwrap().port(), free);
        });
    }

    #[test]
    fn jsonpath_subset() {
        let v = json!({"spec": {"url": "ssh://git", "ports": [{"port": 80}, {"port": 443}]},
            "status": {"conditions": [{"type": "Reconciling", "status": "False"}, {"type": "Ready", "status": "True", "message": "Applied revision: main@sha1:abc"}]}});
        assert_eq!(jsonpath(&v, ".spec.url"), "ssh://git");
        assert_eq!(jsonpath(&v, ".status.conditions[?(@.type==\"Ready\")].status"), "True");
        assert_eq!(
            jsonpath(&v, ".status.conditions[?(@.type == 'Ready')].message"),
            "Applied revision: main@sha1:abc"
        );
        assert_eq!(jsonpath(&v, ".spec.ports[1].port"), "443");
        assert_eq!(jsonpath(&v, ".spec.ports[*].port"), "80,443");
        assert_eq!(jsonpath(&v, ".spec.missing"), "");
    }

    #[test]
    fn quantities() {
        assert_eq!(quantity("250m"), Some(0.25));
        assert_eq!(quantity("2"), Some(2.));
        assert_eq!(quantity("128Mi"), Some(134217728.));
        assert_eq!(quantity("1630100Ki"), Some(1630100. * 1024.));
        assert!((quantity("137131461n").unwrap() - 0.137131461).abs() < 1e-12);
        assert_eq!(quantity("1e3"), Some(1000.));
        assert_eq!(quantity("node-1"), None);
        assert_eq!(quantity("Mi"), None);
        assert_eq!(quantity("1/1"), None);
    }

    #[test]
    fn history_caps_and_forgets() {
        let mut h = History::default();
        let m = |pods: &[&str]| Metrics {
            pods: pods
                .iter()
                .map(|p| ((String::new(), p.to_string()), (Usage { cpu: 1., mem: 2. }, vec![])))
                .collect(),
            nodes: HashMap::new(),
        };
        for _ in 0..HISTORY_LEN + 5 {
            h.record(&m(&["a", "b"]));
        }
        assert_eq!(h.pods[&(String::new(), "a".into())].len(), HISTORY_LEN);
        h.record(&m(&["a"]));
        assert!(!h.pods.contains_key(&(String::new(), "b".into())), "gone pods are dropped");
    }
}

/// Smoke test against your current kubeconfig context: `cargo test -- --ignored`.
#[test]
#[ignore]
fn live_watch_all_kinds() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ctxs, current, _) = load_contexts(&kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("current context");
    let _watchers: Vec<_> = (0..BUILTIN.len()).map(|i| watch(ctx.clone(), i)).collect();
    for _ in 0..150 {
        if CACHE.lock().unwrap().values().filter(|s| s.synced || s.error.is_some()).count() == BUILTIN.len() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let cache = CACHE.lock().unwrap();
    for (i, kind) in BUILTIN.iter().enumerate() {
        let slot = &cache[&(ctx.clone(), i)];
        assert!(slot.synced, "{}: {:?}", kind.name, slot.error);
        let r = slot.rows.values().next();
        println!("{:<12} {:>4}  {:?}", kind.name, slot.rows.len(), r.map(|r| (&r.name, &r.cells)));
    }
    let m = RT.block_on(fetch_metrics(&ctx)).expect("metrics-server");
    println!("node usage: {:?}", m.nodes);
    let pods = &cache[&(ctx.clone(), BUILTIN.iter().position(|k| k.name == "Pods").unwrap())];
    for node in m.nodes.keys() {
        let mut req = Usage::default();
        pods.rows
            .values()
            .filter(|p| s(&p.obj, "/spec/nodeName") == *node && s(&p.obj, "/status/phase") == "Running")
            .for_each(|p| req += pod_resources(&p.obj).0);
        println!("{node} requests: {:.0}m cpu, {:.0}Mi mem", req.cpu * 1000., req.mem / 1048576.);
    }
}

/// Server-side dry run of every quick action against your current context: `cargo test -- --ignored`.
#[test]
#[ignore]
fn live_actions_dry_run() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ctxs, current, _) = load_contexts(&kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("current context");
    let kind = |k: &str| BUILTIN.iter().find(|x| x.kind == k).unwrap();
    let first = |k: &str| {
        let api = kind(k).api(RT.block_on(client(&ctx)).unwrap(), "");
        let o = RT
            .block_on(api.list(&Default::default()))
            .unwrap()
            .items
            .into_iter()
            .next()
            .expect(k);
        (o.metadata.namespace.unwrap_or_default(), o.metadata.name.unwrap_or_default())
    };
    let (ns, cj) = first("CronJob");
    let job = RT.block_on(trigger(ctx.clone(), ns, cj, true)).expect("trigger");
    println!("trigger ok → {job}");
    let (ns, name) = first("Deployment");
    RT.block_on(restart(ctx.clone(), kind("Deployment"), ns.clone(), name.clone(), true))
        .expect("redeploy");
    // Editor round trip: the YAML we show (minus managedFields) must be accepted back as-is.
    for k in ["Deployment", "DaemonSet"] {
        let (ns, name) = first(k);
        let api = kind(k).api(RT.block_on(client(&ctx)).unwrap(), &ns);
        let mut obj = serde_json::to_value(RT.block_on(api.get(&name)).unwrap()).unwrap();
        obj["metadata"].as_object_mut().unwrap().remove("managedFields");
        let yaml = serde_yaml::to_string(&obj).unwrap();
        RT.block_on(replace(ctx.clone(), kind(k), ns, name.clone(), yaml, true))
            .expect("replace");
        println!("replace {k} {name} ok");
    }
    let broken = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: x\nspec: [";
    assert!(
        RT.block_on(replace(ctx.clone(), kind("Deployment"), ns, name, broken.into(), true))
            .is_err()
    );
    for k in ["Job", "Pod"] {
        let (ns, name) = first(k);
        RT.block_on(delete(ctx.clone(), kind(k), ns, name.clone(), true)).expect("delete");
        println!("delete {k} {name} ok");
    }
}

/// CRD discovery + Flux columns + Flux actions (dry run) against your current context: `cargo test -- --ignored`.
#[test]
#[ignore]
fn live_crds_and_flux() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ctxs, current, _) = load_contexts(&kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("current context");
    let found = RT.block_on(discover(ctx.clone())).expect("discover");
    println!("{} CRD kinds", found.len());
    let Some(ks) = find_kind(|k| k.kind == "Kustomization" && k.is_flux()) else {
        return println!("no Flux here");
    };
    let k = kind(ks);
    println!("{} ({}/{}) cols {:?}", k.name, k.group, k.version, k.cols);
    let api = k.api(RT.block_on(client(&ctx)).unwrap(), "");
    let items = RT.block_on(api.list(&Default::default())).unwrap().items;
    for o in items.iter().take(3) {
        let r = row(&ctx, k, o.clone());
        println!("  {}/{} {:?}", r.ns, r.name, r.cells);
        assert!(!r.cells.is_empty() && !r.cells[0].is_empty(), "Ready column empty");
    }
    let o = items.first().expect("a kustomization");
    let (ns, name) = (
        o.metadata.namespace.clone().unwrap_or_default(),
        o.metadata.name.clone().unwrap_or_default(),
    );
    RT.block_on(patch(ctx.clone(), k, ns.clone(), name.clone(), reconcile_patch(), true))
        .expect("reconcile");
    RT.block_on(patch(ctx, k, ns, name, json!({"spec": {"suspend": true}}), true))
        .expect("suspend");
    println!("reconcile + suspend dry runs ok");
}

/// Port-forward round trip against your current context: finds a pod with a TCP containerPort,
/// forwards it and sends an HTTP request through the tunnel. `cargo test -- --ignored`.
#[test]
#[ignore]
fn live_port_forward() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (ctxs, current, _) = load_contexts(&kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("current context");
    let pods = RT.block_on(async {
        Api::<Pod>::all(client(&ctx).await.unwrap())
            .list(&Default::default())
            .await
            .unwrap()
    });
    let (ns, pod, port) = pods
        .items
        .iter()
        .filter(|p| p.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Running"))
        .find_map(|p| {
            let port = p
                .spec
                .as_ref()?
                .containers
                .iter()
                .flat_map(|c| c.ports.iter().flatten())
                .find(|p| p.protocol.as_deref().unwrap_or("TCP") == "TCP")?;
            Some((p.metadata.namespace.clone()?, p.metadata.name.clone()?, port.container_port as u16))
        })
        .expect("a pod with a TCP port");
    let fwd = RT
        .block_on(port_forward(ctx, ns.clone(), pod.clone(), port, 0, false))
        .expect("forward");
    let reply = RT.block_on(async {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", fwd.local)).await.unwrap();
        s.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n").await.unwrap();
        let mut buf = vec![0; 256];
        let n = tokio::time::timeout(Duration::from_secs(10), s.read(&mut buf))
            .await
            .expect("reply in time")
            .unwrap();
        String::from_utf8_lossy(&buf[..n]).to_string()
    });
    println!("{ns}/{pod}:{port} via localhost:{} -> {:?}", fwd.local, reply.lines().next());
    assert!(!reply.is_empty(), "no bytes back through the tunnel");
}
