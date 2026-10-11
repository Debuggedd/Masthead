mod k8s;
mod update;

use gpui::{
    AnyElement, AnyView, App, Application, AssetSource, Bounds, ClipboardItem, Context, Div, ElementId, Entity, FocusHandle, Focusable,
    FontWeight, Hsla, KeyBinding, KeyDownEvent, ListHorizontalSizingBehavior, MouseButton, MouseDownEvent, PathPromptOptions,
    ScrollStrategy, SharedString, Stateful, Svg, Task, TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions,
    actions, div, point, prelude::*, px, relative, rems, size, svg, uniform_list,
};
use gpui_component::{
    Root, Sizable, Theme, ThemeMode,
    highlighter::{LanguageConfig, LanguageRegistry},
    input::{Input, InputEvent, InputState, SelectAll},
    menu::{ContextMenuExt, PopupMenuItem},
};
use k8s::{CACHE, Ctx, Kind, METRICS, Metrics, RT, Row, Usage, VERSION, kind as kind_at, kinds};
use serde_json::Value;
use std::{
    borrow::Cow,
    cmp::Ordering,
    collections::HashMap,
    sync::{Arc, atomic::Ordering::Relaxed},
    time::{Duration, Instant},
};
use tokio::task::JoinHandle;

actions!(masthead, [Quit, Search, CheckForUpdates]);

/// Live-cluster tests share the global cache, so they run one at a time.
#[cfg(test)]
static LIVE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Follows macOS appearance (see `apply_theme`). Colors below are the dark palette; `rgb` maps
/// each to its light counterpart while this is set, so call sites stay `rgb(TEXT)` etc.
static LIGHT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn rgb(c: u32) -> gpui::Rgba {
    gpui::rgb(if LIGHT.load(Relaxed) { light_of(c) } else { c })
}

/// Light counterpart of a dark-palette color (unknown colors, e.g. white text on a blue button, pass through).
fn light_of(c: u32) -> u32 {
    match c {
        0x17181f => 0xf6f7fb, // BG
        0x111218 => 0xeceef4, // PANEL
        0x0e0f14 => 0xffffff, // CODE
        0x1f2029 => 0xffffff, // SURFACE
        0x262833 => 0xdcdfe8, // BORDER
        0x1c1d25 => 0xeceef3, // ROWLINE
        0x1f212b => 0xe6e9f2, // HOVER
        0x243049 => 0xd6e2fc, // SELECT
        0xe3e5ee => 0x1d2130, // TEXT
        0x9a9eb2 => 0x555b70, // DIM
        0x62667c => 0x8a8fa3, // FAINT
        0x7aa2f7 => 0x2f5fe0, // ACCENT
        0xf7768e => 0xcf2f4c, // RED
        0x9ece6a => 0x2e8540, // GREEN
        0xe0af68 => 0xb06f12, // YELLOW
        0xbb9af7 => 0x8150d6, // context colors
        0x7dcfff => 0x0b88b8,
        0xff9e64 => 0xd2611b,
        0xf7a8d8 => 0xc4458f,
        0x73daca => 0x14907e,
        0x2c2f3d => 0xe2e5ed, // bar track
        0x34374a => 0xd2d6e1, // popover border
        0x3a3d50 => 0xc3c8d6, // hover border
        0x2a1a22 => 0xfdecef, // error notice
        0x4a2433 => 0xf2b8c3,
        0x2a2618 => 0xfdf4e3, // warning notice
        0x3d59a1 => 0xc7d6fb, // filter selection / dirty save button
        0x2f4475 => 0xc7d6fb, // editor selection
        other => other,
    }
}

/// Picks the palette from the window's appearance and themes gpui-component (editor, menus) to match.
fn apply_theme(window: &mut Window, cx: &mut App) {
    // MASTHEAD_APPEARANCE=light|dark overrides macOS (screenshots).
    let light = match std::env::var("MASTHEAD_APPEARANCE").as_deref() {
        Ok("light") => true,
        Ok("dark") => false,
        _ => matches!(
            window.appearance(),
            gpui::WindowAppearance::Light | gpui::WindowAppearance::VibrantLight
        ),
    };
    LIGHT.store(light, Relaxed);
    Theme::change(if light { ThemeMode::Light } else { ThemeMode::Dark }, Some(window), cx);
    let theme = Theme::global_mut(cx);
    theme.background = rgb(CODE).into();
    theme.foreground = rgb(TEXT).into();
    theme.border = rgb(BORDER).into();
    theme.input = rgb(BORDER).into();
    theme.caret = rgb(ACCENT).into();
    theme.selection = rgb(0x2f4475).into();
    theme.muted_foreground = rgb(FAINT).into();
    theme.popover = rgb(SURFACE).into();
    theme.popover_foreground = rgb(TEXT).into();
    theme.accent = rgb(SELECT).into();
    theme.accent_foreground = rgb(TEXT).into();
    theme.mono_font_family = "Menlo".into();
    theme.mono_font_size = px(12.);
}

const BG: u32 = 0x17181f;
const PANEL: u32 = 0x111218;
const CODE: u32 = 0x0e0f14;
const SURFACE: u32 = 0x1f2029;
const BORDER: u32 = 0x262833;
const ROWLINE: u32 = 0x1c1d25;
const HOVER: u32 = 0x1f212b;
const SELECT: u32 = 0x243049;
const TEXT: u32 = 0xe3e5ee;
const DIM: u32 = 0x9a9eb2;
const FAINT: u32 = 0x62667c;
const ACCENT: u32 = 0x7aa2f7;
const RED: u32 = 0xf7768e;
const GREEN: u32 = 0x9ece6a;
const YELLOW: u32 = 0xe0af68;
/// Per-context colors, so rows from different clusters are easy to tell apart.
const CTX_COLORS: [u32; 6] = [0x7aa2f7, 0xbb9af7, 0x7dcfff, 0xff9e64, 0xf7a8d8, 0x73daca];
const SIDEBAR_W: f32 = 240.;
/// Table columns dropped first when the window gets narrow (Name is never dropped).
const DROP_ORDER: &[&str] = &[
    "Up-to-date",
    "Available",
    "Node",
    "Restarts",
    "Cluster IP",
    "Ports",
    "Class",
    "Keys",
    "Suspend",
    "Active",
    "Desired",
    "Completions",
    "Capacity",
    "Object",
    "Reason",
    "Image",
    "Roles",
    "Hosts",
    "Type",
    "Schedule",
    "Pods",
    "Memory",
    "Context",
    "Namespace",
    "Ready",
    "Message",
    "CPU",
    "Age",
    "Status",
];
/// Key for a context's metrics poller in `Explorer::watchers`.
const METRICS_SLOT: usize = usize::MAX;
/// Flux kinds in the sidebar, most used first; other Flux kinds follow alphabetically.
const FLUX_ORDER: &[&str] = &[
    "Kustomization",
    "HelmRelease",
    "GitRepository",
    "OCIRepository",
    "HelmRepository",
    "HelmChart",
    "Bucket",
];
/// Persisted sidebar layout: {"expanded": [api groups], "collapsed": [sections]}.
type SidebarState = HashMap<String, std::collections::HashSet<String>>;

/// Sidebar section a kind is listed under.
fn section_of(ix: usize) -> &'static str {
    let kind = kind_at(ix);
    if kind.is_flux() {
        "FLUX"
    } else if kind.custom {
        "CUSTOM RESOURCES"
    } else {
        GROUPS
            .iter()
            .rev()
            .find(|(_, _, start)| *start <= ix)
            .map_or("", |(label, ..)| label)
    }
}

/// Height of the transparent titlebar strip (traffic lights live here).
const TOP: f32 = 44.;
/// Sidebar group headers: (label, icon, index of the first built-in kind in the group).
const GROUPS: [(&str, &str, usize); 5] = [
    ("CLUSTER", "server", 0),
    ("WORKLOADS", "layers", 3),
    ("NETWORK", "network", 9),
    ("CONFIG", "sliders-horizontal", 11),
    ("STORAGE", "archive", 13),
];

/// Icons are embedded so the binary stays self-contained.
macro_rules! icons {
    ($($name:literal),* $(,)?) => {
        fn icon_bytes(path: &str) -> Option<&'static [u8]> {
            match path {
                $(concat!("icons/", $name, ".svg") => Some(include_bytes!(concat!("../assets/icons/", $name, ".svg"))),)*
                _ => None,
            }
        }
    };
}
icons!(
    "activity",
    "archive",
    "arrow-left",
    "box",
    "check",
    "chevrons-up-down",
    "clock",
    "copy",
    "cpu",
    "database",
    "file-code",
    "file-text",
    "folder",
    "globe",
    "hard-drive",
    "key-round",
    "layers",
    "layout-dashboard",
    "network",
    "play",
    "plus",
    "rocket",
    "rotate-ccw",
    "scroll-text",
    "search",
    "server",
    "share-2",
    "sliders-horizontal",
    "trash-2",
    "x",
    "zap",
    "ship",
    "git-branch",
    "package",
    "anchor",
    "puzzle",
    "file-stack",
    "send",
    "radio-receiver",
    "pause",
    "refresh-cw",
    "chevron-right",
    "chevron-down",
    "arrow-left-right",
    "square",
    "triangle-alert",
    "circle-check",
    "download",
);

struct Assets;

impl AssetSource for Assets {
    /// Our icons first, then gpui-component's (its editor search bar and menus use those).
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        match icon_bytes(path) {
            Some(b) => Ok(Some(Cow::Borrowed(b))),
            None => gpui_component_assets::Assets.load(path),
        }
    }

    fn list(&self, _: &str) -> anyhow::Result<Vec<SharedString>> {
        Ok(vec![])
    }
}

/// SVG color doesn't inherit from the parent, so it's always explicit.
fn icon(name: &str, color: u32) -> Svg {
    svg()
        .path(format!("icons/{name}.svg"))
        .flex_none()
        .size(px(15.))
        .text_color(rgb(color))
}

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Overview,
    Yaml,
    Logs,
    Events,
    Map,
}

/// Kinds with a Map tab (the Ingress → Service → workload → Pods → config chain).
fn has_map(kind: &str) -> bool {
    matches!(
        kind,
        "Ingresses" | "Services" | "Deployments" | "StatefulSets" | "DaemonSets" | "Jobs" | "CronJobs" | "Pods"
    )
}

/// One box on the map. `missing`: referenced but not found (shown red, not clickable).
struct MapNode {
    kind: usize,
    name: String,
    sub: String,
    color: u32,
    missing: bool,
}

type Key = (Ctx, String, String);

#[derive(Clone, Copy, PartialEq)]
enum Action {
    Delete,
    Redeploy,
    Trigger,
    Reconcile,
    ReconcileWithSource,
    Suspend,
    Resume,
}

impl Action {
    /// Kind-specific actions (Delete is added separately). `suspended` picks Suspend vs Resume for Flux.
    fn quick(kind: &Kind, suspended: bool) -> Vec<Action> {
        if kind.is_flux() {
            let with_source = matches!(kind.kind, "Kustomization" | "HelmRelease");
            let mut actions = vec![Action::Reconcile];
            actions.extend(with_source.then_some(Action::ReconcileWithSource));
            actions.push(if suspended { Action::Resume } else { Action::Suspend });
            return actions;
        }
        match kind.name {
            "Pods" | "Jobs" => vec![Action::Delete],
            "Deployments" | "StatefulSets" | "DaemonSets" => vec![Action::Redeploy],
            "CronJobs" => vec![Action::Trigger],
            _ => vec![],
        }
    }

    fn label(self) -> &'static str {
        match self {
            Action::Delete => "Delete",
            Action::Redeploy => "Redeploy",
            Action::Trigger => "Trigger now",
            Action::Reconcile => "Reconcile",
            Action::ReconcileWithSource => "Reconcile with source",
            Action::Suspend => "Suspend",
            Action::Resume => "Resume",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Action::Delete => "trash-2",
            Action::Redeploy => "rotate-ccw",
            Action::Trigger => "zap",
            Action::Reconcile => "refresh-cw",
            Action::ReconcileWithSource => "git-branch",
            Action::Suspend => "pause",
            Action::Resume => "play",
        }
    }

    fn color(self) -> u32 {
        if self == Action::Delete { RED } else { DIM }
    }
}

struct YamlEditor {
    state: Entity<InputState>,
    /// Object version the text was loaded from, and that text (to detect unsaved edits).
    base: Arc<Row>,
    original: String,
    /// Object changed on the cluster while the editor was in use.
    stale: bool,
    /// Take the next cluster update even if focused (it's our own save coming back).
    reload_next: bool,
    saving: bool,
    error: Option<String>,
}

impl YamlEditor {
    fn dirty(&self, cx: &App) -> bool {
        self.state.read(cx).value() != self.original.as_str()
    }
}

/// Editable YAML for an object: everything except server-managed field ownership noise.
fn yaml_text(row: &Row) -> String {
    let mut obj = row.obj.clone();
    if let Some(meta) = obj.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.remove("managedFields");
    }
    serde_yaml::to_string(&obj).unwrap_or_else(|e| e.to_string())
}

/// Right-click menu for a table row: open it, then its actions (destructive last).
type MenuFn = Box<dyn Fn(&mut Explorer, &mut Window, &mut Context<Explorer>)>;

/// Right-click menu for a table row: a header naming the object, then Open / Tools / Actions groups,
/// with tinted icons, details on the right, and Delete last in red.
fn row_menu(
    menu: gpui_component::menu::PopupMenu,
    view: gpui::WeakEntity<Explorer>,
    kind: usize,
    key: Key,
    suspended: bool,
    urls: Vec<String>,
    window: &mut Window,
    cx: &mut Context<gpui_component::menu::PopupMenu>,
) -> gpui_component::menu::PopupMenu {
    // A row: tinted icon, label, optional dim detail on the right.
    let entry = |label: String, detail: String, icon_name: &'static str, tint: u32, danger: bool, f: MenuFn| {
        let view = view.clone();
        PopupMenuItem::element(move |_, _| {
            div()
                .flex()
                .items_center()
                .gap_2p5()
                .w_full()
                .py(px(3.))
                .child(icon(icon_name, tint).size(px(15.)))
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(if danger { RED } else { TEXT }))
                        .child(label.clone()),
                )
                .child(div().flex_1())
                .when(!detail.is_empty(), |d| {
                    d.child(
                        div()
                            .flex_none()
                            .max_w(px(160.))
                            .truncate()
                            .text_xs()
                            .text_color(rgb(FAINT))
                            .child(detail.clone()),
                    )
                })
        })
        .on_click(move |_, window, cx| {
            view.update(cx, |this, cx| f(this, window, cx)).ok();
        })
    };
    let open = |tab: Tab| -> MenuFn {
        let key = key.clone();
        Box::new(move |this, _, cx| this.open_tab(kind, key.clone(), tab, cx))
    };
    let k = kind_at(kind);
    let (shell, forwardable, forward_first) = view
        .upgrade()
        .and_then(|v| {
            let this = v.read(cx);
            let row = this
                .rows
                .iter()
                .find(|r| r.ctx == key.0 && r.ns == key.1 && r.name == key.2)?
                .clone();
            // Resolved only when the menu opens (Services/workloads scan pods).
            let targets = this.forward_targets(&row);
            let ports: Vec<String> = targets.iter().map(|t| t.want.to_string()).collect();
            let first = targets.first().map(|t| t.want);
            Some((this.shell_target(&row), (!ports.is_empty()).then(|| (row, ports.join(", "))), first))
        })
        .unwrap_or((None, None, None));

    // Header: object name with its kind, namespace and context.
    let (name, sub) = (key.2.clone(), {
        let mut parts = vec![k.kind.to_string()];
        parts.extend([key.1.clone(), key.0.name.clone()].into_iter().filter(|s| !s.is_empty()));
        parts.join(" · ")
    });
    let mut menu = menu
        .min_w(px(280.))
        .item(
            PopupMenuItem::element(move |_, _| {
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .py_1()
                    .child(icon(k.icon, ACCENT).size(px(18.)))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .child(
                                div()
                                    .max_w(px(240.))
                                    .truncate()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(TEXT))
                                    .child(name.clone()),
                            )
                            .child(div().max_w(px(240.)).truncate().text_xs().text_color(rgb(FAINT)).child(sub.clone())),
                    )
            })
            .disabled(true),
        )
        .separator();

    // Open
    menu = menu
        .item(entry(
            "Overview".into(),
            String::new(),
            "layout-dashboard",
            ACCENT,
            false,
            open(Tab::Overview),
        ))
        .item(entry(
            "Edit YAML".into(),
            String::new(),
            "file-code",
            ACCENT,
            false,
            open(Tab::Yaml),
        ))
        .item(entry("Events".into(), String::new(), "activity", ACCENT, false, open(Tab::Events)));
    if has_map(k.name) {
        menu = menu.item(entry("App map".into(), String::new(), "map", ACCENT, false, open(Tab::Map)));
    }
    if has_logs(k.name) {
        menu = menu.item(entry("Logs".into(), String::new(), "scroll-text", ACCENT, false, open(Tab::Logs)));
    }

    // Tools
    menu = menu.separator();
    let shell_cmd = shell.clone();
    let forward_cmd = forward_first;
    if let Some((pod, container)) = shell {
        let (ctx, ns) = (key.0.clone(), key.1.clone());
        menu = menu.item(entry(
            "Open shell".into(),
            container.clone(),
            "square-terminal",
            GREEN,
            false,
            Box::new(move |this, _, cx| {
                this.status = open_shell(&ctx, &ns, &pod, &container);
                cx.notify();
            }),
        ));
    }
    if let Some((row, ports)) = forwardable {
        menu = menu.item(entry(
            "Port forward…".into(),
            ports,
            "arrow-left-right",
            0x7dcfff,
            false,
            Box::new(move |this, _, cx| this.open_fwd_modal(row.clone(), 0, cx)),
        ));
    }
    for url in urls {
        let host = url.split("://").nth(1).unwrap_or(&url).to_string();
        menu = menu.item(entry(
            "Open in browser".into(),
            host,
            "globe",
            0x7dcfff,
            false,
            Box::new(move |_, _, cx| cx.open_url(&url)),
        ));
    }
    let copy = key.2.clone();
    menu = menu.item(entry(
        "Copy name".into(),
        String::new(),
        "copy",
        DIM,
        false,
        Box::new(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
    ));
    let commands = kubectl_commands(k, &key, shell_cmd.as_ref(), forward_cmd.as_ref());
    let sub_view = view.clone();
    menu = menu.submenu("Copy as kubectl", window, cx, move |sub, _, _| {
        commands.iter().fold(sub.min_w(px(240.)), |sub, (label, cmd)| {
            let (cmd, view, label) = (cmd.clone(), sub_view.clone(), *label);
            let shown = cmd.clone();
            sub.item(
                PopupMenuItem::element(move |_, _| {
                    div()
                        .flex()
                        .flex_col()
                        .py(px(3.))
                        .child(div().text_color(rgb(TEXT)).child(label))
                        .child(
                            div()
                                .max_w(px(420.))
                                .truncate()
                                .font_family("Menlo")
                                .text_xs()
                                .text_color(rgb(FAINT))
                                .child(shown.clone()),
                        )
                })
                .on_click(move |_, _, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(cmd.clone()));
                    view.update(cx, |this, cx| {
                        this.status = format!("Copied: {cmd}");
                        cx.notify();
                    })
                    .ok();
                }),
            )
        })
    });

    // Actions, then Delete on its own.
    let actions: Vec<Action> = Action::quick(k, suspended).into_iter().filter(|a| *a != Action::Delete).collect();
    if !actions.is_empty() {
        menu = menu.separator();
    }
    for a in actions.into_iter().chain([Action::Delete]) {
        if a == Action::Delete {
            menu = menu.separator();
        }
        let key = key.clone();
        let tint = if a == Action::Delete { RED } else { YELLOW };
        menu = menu.item(entry(
            a.label().into(),
            String::new(),
            a.icon(),
            tint,
            a == Action::Delete,
            Box::new(move |this, _, cx| this.act(a, kind, key.clone(), cx)),
        ));
    }
    menu
}

fn suspended(obj: &Value) -> bool {
    obj.pointer("/spec/suspend") == Some(&Value::Bool(true))
}

/// Plain text tooltip (GPUI tooltips are views).
struct Tip(SharedString);

impl Render for Tip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(0x34374a))
            .text_xs()
            .text_color(rgb(TEXT))
            .child(self.0.clone())
    }
}

fn tip(text: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
    let text = text.into();
    move |_, cx| cx.new(|_| Tip(text.clone())).into()
}
/// (label, value, optional link target as (kind index, key)).
type Fact = (&'static str, String, Option<Link>);

/// A port that can be forwarded: label for humans, port on the pod, preferred local port, and the pod
/// it resolves to right now (Services/workloads pick a ready pod; None if there isn't one).
#[derive(Clone)]
struct FwdTarget {
    label: String,
    remote: u16,
    want: u16,
    pod: Option<String>,
}

/// Port-forward dialog state (Aptakube-style: local port → container port dropdown, open-in-browser).
struct FwdModal {
    row: Arc<Row>,
    /// Selected index into `forward_targets(row)`.
    sel: usize,
    input: Option<Entity<InputState>>,
    /// Selection a free-port suggestion was requested for, and the result waiting to be put in the field.
    asked: Option<usize>,
    suggested: Option<(usize, u16)>,
    focus: bool,
    picking: bool,
    busy: bool,
    error: Option<String>,
}

/// One Helm release (its newest revision), read from Helm's own storage Secrets.
#[derive(Clone)]
struct HelmRel {
    ctx: Ctx,
    ns: String,
    name: String,
    revision: i64,
    status: String,
    /// The storage Secret of this revision (sh.helm.release.v1.<name>.v<revision>).
    secret: Arc<Row>,
}

#[derive(Clone, Copy, PartialEq)]
enum HelmTab {
    Overview,
    Values,
    Defaults,
    Manifest,
    Notes,
}

/// Decodes a Helm release Secret: base64 (Kubernetes) → base64 (Helm) → gzip → JSON. Keeps only what
/// the UI shows: drops the chart's templates and files, and the rendered manifest (often ~1 MB, CRDs)
/// unless `with_manifest`.
fn decode_helm(secret: &Row, with_manifest: bool) -> Option<Value> {
    let helm_b64 = decode_secret(secret.obj.pointer("/data/release")?.as_str()?);
    let raw = serde_json::from_value::<k8s_openapi::ByteString>(Value::String(helm_b64)).ok()?.0;
    let json = if raw.starts_with(&[0x1f, 0x8b]) {
        let mut out = vec![];
        std::io::Read::read_to_end(&mut flate2::read::GzDecoder::new(&raw[..]), &mut out).ok()?;
        out
    } else {
        raw
    };
    let mut v: Value = serde_json::from_slice(&json).ok()?;
    if let Some(chart) = v.get_mut("chart").and_then(Value::as_object_mut) {
        chart.retain(|k, _| k == "metadata" || k == "values");
    }
    if !with_manifest {
        v.as_object_mut()?.remove("manifest");
    }
    Some(v)
}

fn helm_status_color(status: &str) -> u32 {
    match status {
        "deployed" => GREEN,
        "failed" => RED,
        "superseded" | "uninstalled" => FAINT,
        _ => YELLOW, // pending-install/upgrade/rollback, uninstalling, unknown
    }
}

/// Something wrong (or worth knowing) across the selected clusters, for the Problems page.
#[derive(Clone, PartialEq)]
struct Problem {
    /// 0 = error, 1 = warning, 2 = info.
    severity: u8,
    kind: usize,
    key: Key,
    reason: String,
    message: String,
    since: i64,
}

/// (kind, object, container, previous, timestamps): a new stream starts when any of these change.
type LogKey = (usize, Key, Option<String>, bool, bool);

struct LogSession {
    key: LogKey,
    stream: k8s::LogStream,
    seen: u64,
}

/// Kinds with a Logs tab (workloads show all their pods merged).
fn has_logs(kind: &str) -> bool {
    matches!(kind, "Pods" | "Deployments" | "StatefulSets" | "DaemonSets" | "Jobs")
}

/// A running port-forward (stopping = dropping it).
struct FwdEntry {
    ctx: Ctx,
    ns: String,
    /// e.g. "service/api"
    what: String,
    pod: String,
    remote: u16,
    fwd: k8s::Forward,
}

/// Where a clickable fact goes: another object's page, or a URL in the browser.
#[derive(Clone)]
enum Link {
    Object(usize, Key),
    Url(String),
}

/// Requests/limits of the non-finished pods scheduled on a node.
#[derive(Default, Clone, Copy, PartialEq)]
struct Alloc {
    req: Usage,
    lim: Usage,
    pods: usize,
}

fn kind_ix(name: &str) -> usize {
    kinds().iter().position(|k| k.name == name).unwrap_or(0)
}

struct Explorer {
    focus: FocusHandle,
    ctxs: Vec<Ctx>,
    active: Vec<Ctx>,
    /// One watcher per (active context, kind); every page is pre-loaded and live.
    /// Watchers per context and kind (metrics poller under `METRICS_SLOT`). Built-ins and Flux kinds start
    /// eagerly; other CRDs on first view.
    watchers: HashMap<Ctx, HashMap<usize, JoinHandle<()>>>,
    /// CRD kinds each context serves (from discovery).
    available: HashMap<Ctx, Vec<usize>>,
    discovery: HashMap<Ctx, Task<()>>,
    /// Expanded API groups in the sidebar's Custom resources section.
    expanded: std::collections::HashSet<String>,
    /// Folded sidebar sections (CLUSTER, WORKLOADS, …, FLUX, CUSTOM RESOURCES). Both sets persist in sidebar.json.
    collapsed: std::collections::HashSet<String>,
    seen: u64,
    kind: usize,
    rows: Vec<Arc<Row>>,
    view: Vec<usize>,
    errors: Vec<(Ctx, String)>,
    loading: bool,
    filter: String,
    /// Whole filter selected (⌘A / Ctrl+A): next keystroke replaces it.
    filter_all: bool,
    /// Sort per page: kind name -> (column header, ascending). Persisted across restarts.
    sorts: HashMap<String, (String, bool)>,
    selected: Option<Key>,
    tab: Tab,
    container: Option<String>,
    /// Read-only editors for shown ConfigMap/Secret values of the open object, by data key.
    value_views: HashMap<String, Entity<InputState>>,
    /// Secret keys whose values are shown; cleared whenever the selection changes.
    revealed: std::collections::HashSet<String>,
    /// YAML editors by (kind index, object). Dirty ones are kept as drafts when you navigate away.
    editors: HashMap<(usize, Key), YamlEditor>,
    /// Detail-panel navigation stack for "back".
    history: Vec<(usize, Key)>,
    metrics: HashMap<Ctx, Arc<Metrics>>,
    alloc: HashMap<(Ctx, String), Alloc>,
    /// Live log stream for the open Logs tab (dropped when leaving it).
    logs: Option<LogSession>,
    log_follow: bool,
    log_previous: bool,
    log_timestamps: bool,
    log_search: Option<Entity<InputState>>,
    log_scroll: UniformListScrollHandle,
    log_wrap: bool,
    /// Visual log rows matching the filter, as (line index, byte range); a wrapped line spans several rows.
    log_view: Vec<(usize, std::ops::Range<usize>)>,
    /// Row of the longest line (sets the scroll width when not wrapping).
    log_widest: usize,
    list_scroll: UniformListScrollHandle,
    picker_open: bool,
    forwards: Vec<FwdEntry>,
    /// The "Port forward" dialog, when open.
    fwd_modal: Option<FwdModal>,
    /// ⌘P search: the query field (Some = open) and the highlighted result.
    palette: Option<Entity<InputState>>,
    palette_sel: usize,
    palette_scroll: gpui::ScrollHandle,
    palette_query: Option<String>,
    /// Hand keyboard focus back to the app on the next render (an overlay with a focused field closed).
    refocus: bool,
    /// MASTHEAD_KEYS="cmd-p,a,p,i,wait:1500,enter": keystrokes replayed once data is loaded, one every
    /// MASTHEAD_KEY_DELAY ms (default: one per frame); `wait:N` pauses N ms (screenshots, demos).
    pending_keys: std::collections::VecDeque<String>,
    next_key_at: Instant,
    /// Screenshot hook (`MASTHEAD_OPEN=Kind/name#menu`): the row menu for this object, shown pinned.
    debug_menu: Option<(usize, Key)>,
    debug_menu_view: Option<Entity<gpui_component::menu::PopupMenu>>,
    /// Overview (home) page: one summary card per selected cluster. The app opens here.
    home_mode: bool,
    /// Objects opened recently (newest first), shown in ⌘P before you type. Kind by name: CRD indices
    /// can differ between runs.
    recent: std::collections::VecDeque<(&'static str, Key)>,
    /// Rows picked for bulk actions (⌘-click toggles, ⇧-click extends from `pick_anchor`, ⌘A all).
    picked: std::collections::HashSet<Key>,
    pick_anchor: Option<Key>,
    /// Helm releases page / detail.
    helm_mode: bool,
    helm_sel: Option<(Ctx, String, String)>,
    helm_tab: HelmTab,
    /// Decoded release per storage Secret, kept while the Secret is unchanged (UI-thread only).
    helm_memo: std::cell::RefCell<HashMap<(Ctx, String, String), (Arc<Row>, Arc<Value>)>>,
    /// Read-only editor for the Values/Defaults/Manifest tabs, and what it currently shows.
    helm_text: Option<(String, Entity<InputState>)>,
    /// Problems page is showing (instead of a kind's table).
    problems_mode: bool,
    problems: Vec<Problem>,
    /// Opened an object from Problems: back returns there.
    from_problems: bool,
    /// Namespaces to show on namespaced pages (empty = all). Persisted in namespaces.json.
    ns_filter: std::collections::BTreeSet<String>,
    ns_open: bool,
    /// Search field of the namespace picker (created on open; needs the window).
    ns_search: Option<Entity<InputState>>,
    /// Open the forwarded URL in the browser once a forward starts (remembered in settings.json).
    open_browser: bool,
    /// Post a macOS notification for new errors (remembered in settings.json).
    notify: bool,
    /// Errors already seen (notified or baseline), by `problem_id`.
    notified: std::collections::HashSet<String>,
    /// When each context was selected: its first minute of problems is baseline, not "new".
    ctx_since: HashMap<Ctx, Instant>,
    /// MASTHEAD_OPEN="Kind[/name][#overview|yaml|logs|picker]": opens a view once data arrives (for scripts/screenshot.sh).
    pending_open: Option<String>,
    /// Layout computed from the window size each render: visible column indices and detail panel width.
    shown: Vec<usize>,
    /// Width of each flexible (Name/Message) column after fitting.
    flex_w: f32,
    detail_w: f32,
    status: String,
    /// Own CPU/memory, shown in the footer.
    app_stats: String,
    app_stats_tip: String,
    last_sample: (Instant, f64),
    _poll: Task<()>,
    _update_check: Task<()>,
    /// A newer release found by the update check; `updating` while it downloads and installs.
    new_release: Option<update::Release>,
    updating: bool,
}

impl Explorer {
    fn new(cx: &mut Context<Self>) -> Self {
        // Cheap 250ms check of the cache version; re-render only when this page changed.
        // Ages tick every 10s and logs follow every 5s (a redraw costs a few % CPU, so don't do it every second).
        let poll = cx.spawn(async move |this, cx| {
            for tick in 1u64.. {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let alive = this.update(cx, |this, cx| {
                    let v = VERSION.load(Relaxed);
                    if v != this.seen {
                        this.seen = v;
                        if this.sync_rows() {
                            cx.notify();
                        }
                    }
                    let picker = this
                        .pending_open
                        .as_deref()
                        .is_some_and(|s| s.ends_with("#picker") || s.ends_with("#ns"));
                    if this.pending_open.is_some() && (!this.loading || picker) {
                        this.apply_pending_open(cx);
                    }
                    if let Some(session) = this.logs.as_mut() {
                        let v = session.stream.version.load(Relaxed);
                        if v != session.seen {
                            session.seen = v;
                            cx.notify();
                        }
                    }
                    if tick % 20 == 0 && this.sample_stats() {
                        cx.notify();
                    }
                    if !this.pending_keys.is_empty() {
                        cx.notify();
                    }
                    if tick % 40 == 0 {
                        cx.notify();
                    }
                });
                if alive.is_err() {
                    break;
                }
            }
        });
        let mut this = Self {
            focus: cx.focus_handle(),
            ctxs: vec![],
            active: vec![],
            watchers: HashMap::new(),
            available: HashMap::new(),
            discovery: HashMap::new(),
            expanded: load_json::<SidebarState>("sidebar.json").remove("expanded").unwrap_or_default(),
            collapsed: load_json::<SidebarState>("sidebar.json").remove("collapsed").unwrap_or_default(),
            seen: 0,
            kind: kinds().iter().position(|k| k.name == "Pods").unwrap_or(0),
            rows: vec![],
            view: vec![],
            errors: vec![],
            loading: false,
            filter: String::new(),
            filter_all: false,
            sorts: load_json("sort.json"),
            selected: None,
            tab: Tab::Overview,
            container: None,
            editors: HashMap::new(),
            revealed: Default::default(),
            value_views: HashMap::new(),
            history: vec![],
            metrics: HashMap::new(),
            alloc: HashMap::new(),
            logs: None,
            log_follow: true,
            log_previous: false,
            log_timestamps: false,
            log_search: None,
            log_scroll: UniformListScrollHandle::new(),
            log_view: vec![],
            log_widest: 0,
            log_wrap: load_json::<HashMap<String, bool>>("settings.json")
                .get("log_wrap")
                .copied()
                .unwrap_or(false),
            list_scroll: UniformListScrollHandle::new(),
            picker_open: false,
            forwards: vec![],
            fwd_modal: None,
            palette: None,
            palette_sel: 0,
            palette_scroll: gpui::ScrollHandle::new(),
            palette_query: None,
            refocus: false,
            pending_keys: std::env::var("MASTHEAD_KEYS")
                .map(|k| k.split(',').map(String::from).collect())
                .unwrap_or_default(),
            next_key_at: Instant::now(),
            debug_menu: None,
            debug_menu_view: None,
            home_mode: true,
            recent: Default::default(),
            picked: Default::default(),
            pick_anchor: None,
            helm_mode: false,
            helm_sel: None,
            helm_tab: HelmTab::Overview,
            helm_memo: Default::default(),
            helm_text: None,
            problems_mode: false,
            problems: vec![],
            from_problems: false,
            ns_filter: load_json("namespaces.json"),
            ns_open: false,
            ns_search: None,
            open_browser: load_json::<HashMap<String, bool>>("settings.json")
                .get("open_browser")
                .copied()
                .unwrap_or(false),
            notify: load_json::<HashMap<String, bool>>("settings.json")
                .get("notify")
                .copied()
                .unwrap_or(true),
            notified: Default::default(),
            ctx_since: HashMap::new(),
            pending_open: std::env::var("MASTHEAD_OPEN").ok(),
            shown: vec![],
            flex_w: MIN_FLEX_W,
            detail_w: 560.,
            status: String::new(),
            app_stats: String::new(),
            app_stats_tip: String::new(),
            last_sample: (Instant::now(), self_usage().0),
            _poll: poll,
            // Update check shortly after launch, then daily. Screenshot runs skip it unless a test URL is set.
            _update_check: cx.spawn(async move |this, cx| {
                if std::env::var("MASTHEAD_SIZE").is_ok() && std::env::var("MASTHEAD_RELEASES_URL").is_err() {
                    return;
                }
                cx.background_executor().timer(Duration::from_secs(5)).await;
                while this.update(cx, |this, cx| this.check_updates(false, cx)).is_ok() {
                    cx.background_executor().timer(Duration::from_secs(24 * 3600)).await;
                }
            }),
            new_release: None,
            updating: false,
        };
        this.reload_contexts(cx);
        this
    }

    /// Reads the kubeconfigs off the UI thread, then applies them. Kubeconfigs may live in protected
    /// folders (Documents, Desktop, …): macOS blocks the read until its permission prompt is answered,
    /// and a prompt stealing focus while the window is being created deadlocks GPUI 0.2.2
    /// (`window_did_change_key_status` re-enters itself while holding the window lock).
    /// Looks for a newer release. Manual checks (menu, footer version) report every outcome in the status bar.
    fn check_updates(&mut self, manual: bool, cx: &mut Context<Self>) {
        if manual {
            self.status = "Checking for updates…".into();
            cx.notify();
        }
        cx.spawn(async move |this, cx| {
            let found = cx.background_executor().spawn(async { update::check() }).await;
            this.update(cx, |this, cx| {
                match found {
                    Ok(Some(release)) => {
                        if manual {
                            this.status = format!("Masthead {} is available", release.version);
                        }
                        this.new_release = Some(release);
                    }
                    Ok(None) if manual => this.status = format!("Masthead is up to date (v{})", env!("CARGO_PKG_VERSION")),
                    Err(e) if manual => this.status = format!("Update check failed: {e}"),
                    _ => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Downloads the new release, swaps the app bundle and relaunches. Outside an .app (cargo run) it
    /// opens the release page instead.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let Some(release) = self.new_release.clone().filter(|_| !self.updating) else {
            return;
        };
        let Some(app) = update::current_app() else {
            cx.open_url(&release.page);
            return;
        };
        self.updating = true;
        self.status = format!("Downloading Masthead {}…", release.version);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let done = cx
                .background_executor()
                .spawn(async move { update::install(&release.zip, &app).and_then(|_| update::relaunch(&app)) })
                .await;
            this.update(cx, |this, cx| match done {
                Ok(()) => cx.quit(),
                Err(e) => {
                    this.updating = false;
                    this.status = format!("Update failed: {e}");
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn reload_contexts(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_executor()
                .spawn(async { k8s::load_contexts(&k8s::kubeconfig_files()) })
                .await;
            this.update(cx, |this, cx| {
                this.apply_contexts(loaded, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn apply_contexts(&mut self, (ctxs, current, errs): (Vec<Ctx>, Option<String>, Vec<String>), cx: &mut Context<Self>) {
        self.status = errs.join(" · ");
        if self.active.is_empty() {
            // Last session's selection, else the kubeconfig's current context.
            let saved: Vec<(String, String)> = load_json("contexts.json");
            self.active = ctxs
                .iter()
                .filter(|c| saved.contains(&(c.name.clone(), c.file.display().to_string())))
                .cloned()
                .collect();
            if self.active.is_empty() {
                self.active = ctxs.iter().filter(|c| Some(&c.name) == current.as_ref()).take(1).cloned().collect();
            }
        }
        self.active.retain(|c| ctxs.contains(c));
        self.ctxs = ctxs;
        self.sync_watchers(cx);
    }

    /// Start watchers for newly active contexts, stop (and forget) the rest.
    fn sync_watchers(&mut self, cx: &mut Context<Self>) {
        let active = self.active.clone();
        self.watchers.retain(|ctx, handles| {
            let keep = active.contains(ctx);
            if !keep {
                handles.values().for_each(JoinHandle::abort);
                k8s::forget(ctx);
            }
            keep
        });
        self.available.retain(|ctx, _| active.contains(ctx));
        self.ctx_since.retain(|ctx, _| active.contains(ctx));
        self.helm_memo.borrow_mut().retain(|k, _| active.contains(&k.0));
        self.discovery.retain(|ctx, _| active.contains(ctx));
        for ctx in &active {
            if self.watchers.contains_key(ctx) {
                continue;
            }
            let mut handles: HashMap<usize, JoinHandle<()>> = (0..k8s::BUILTIN_COUNT).map(|i| (i, k8s::watch(ctx.clone(), i))).collect();
            handles.insert(METRICS_SLOT, k8s::watch_metrics(ctx.clone()));
            self.watchers.insert(ctx.clone(), handles);
            self.ctx_since.insert(ctx.clone(), Instant::now());
            self.discover(ctx.clone(), cx);
        }
        self.sync_rows();
    }

    /// Finds the context's CRDs, then watches its Flux kinds right away (and the open page, if custom).
    fn discover(&mut self, ctx: Ctx, cx: &mut Context<Self>) {
        let task = cx.spawn({
            let ctx = ctx.clone();
            async move |this, cx| {
                let res = run(k8s::discover(ctx.clone())).await;
                this.update(cx, |this, cx| {
                    match res {
                        Ok(kinds) => {
                            this.available.insert(ctx.clone(), kinds.clone());
                            let current = this.kind;
                            for ix in kinds.into_iter().filter(|&ix| kind_at(ix).is_flux() || ix == current) {
                                this.watch_kind(&ctx, ix);
                            }
                        }
                        Err(e) => this.status = format!("{}: couldn't list CRDs: {e:#}", ctx.name),
                    }
                    this.sync_rows();
                    cx.notify();
                })
                .ok();
            }
        });
        self.discovery.insert(ctx, task);
    }

    fn watch_kind(&mut self, ctx: &Ctx, ix: usize) {
        if let Some(handles) = self.watchers.get_mut(ctx) {
            handles.entry(ix).or_insert_with(|| k8s::watch(ctx.clone(), ix));
        }
    }

    /// Does this context serve the kind? Built-ins always; CRDs once discovered there.
    fn serves(&self, ctx: &Ctx, ix: usize) -> bool {
        ix < k8s::BUILTIN_COUNT || self.available.get(ctx).is_some_and(|k| k.contains(&ix))
    }

    fn reconnect(&mut self, cx: &mut Context<Self>) {
        for (ctx, handles) in self.watchers.drain() {
            handles.values().for_each(JoinHandle::abort);
            k8s::forget(&ctx);
        }
        self.available.clear();
        self.sync_watchers(cx);
        self.status = "Reconnected".into();
    }

    /// Pull the current kind's rows for all active contexts out of the live cache.
    /// Returns whether anything visible changed.
    fn sync_rows(&mut self) -> bool {
        let cache = CACHE.lock().unwrap();
        let old = (std::mem::take(&mut self.rows), std::mem::take(&mut self.errors), self.loading);
        self.loading = false;
        for ctx in &self.active {
            let discovering = self.kind >= k8s::BUILTIN_COUNT && !self.available.contains_key(ctx);
            if !self.serves(ctx, self.kind) {
                self.loading |= discovering;
                continue;
            }
            match cache.get(&(ctx.clone(), self.kind)) {
                Some(slot) => {
                    let all_ns = self.ns_filter.is_empty() || !kind_at(self.kind).namespaced;
                    self.rows
                        .extend(slot.rows.values().filter(|r| all_ns || self.ns_filter.contains(&r.ns)).cloned());
                    if let Some(e) = &slot.error {
                        self.errors.push((ctx.clone(), e.clone()));
                    }
                    self.loading |= !slot.synced && slot.error.is_none();
                }
                None => self.loading = true,
            }
        }
        let mut alloc: HashMap<(Ctx, String), Alloc> = HashMap::new();
        for ctx in &self.active {
            for p in cache.get(&(ctx.clone(), kind_ix("Pods"))).into_iter().flat_map(|s| s.rows.values()) {
                let (node, phase) = (k8s::s(&p.obj, "/spec/nodeName"), k8s::s(&p.obj, "/status/phase"));
                if node.is_empty() || phase == "Succeeded" || phase == "Failed" {
                    continue;
                }
                let (req, lim) = k8s::pod_resources(&p.obj);
                let a = alloc.entry((ctx.clone(), node)).or_default();
                a.req += req;
                a.lim += lim;
                a.pods += 1;
            }
        }
        if !self.picked.is_empty() {
            let present: std::collections::HashSet<Key> = self.rows.iter().map(|r| (r.ctx.clone(), r.ns.clone(), r.name.clone())).collect();
            self.picked.retain(|k| present.contains(k));
        }
        let problems = self.find_problems(&cache);
        drop(cache);
        let problems_changed = problems != self.problems;
        self.problems = problems;
        if problems_changed {
            self.notify_new_problems();
        }
        let metrics: HashMap<Ctx, Arc<Metrics>> = {
            let m = METRICS.lock().unwrap();
            self.active.iter().filter_map(|c| Some((c.clone(), m.get(c)?.clone()))).collect()
        };
        let metrics_changed =
            metrics.len() != self.metrics.len() || metrics.iter().any(|(c, m)| !self.metrics.get(c).is_some_and(|o| Arc::ptr_eq(o, m)));
        let alloc_changed = alloc != self.alloc;
        self.metrics = metrics;
        self.alloc = alloc;
        old.0.len() != self.rows.len()
            || !old.0.iter().zip(&self.rows).all(|(a, b)| Arc::ptr_eq(a, b))
            || old.1 != self.errors
            || old.2 != self.loading
            || metrics_changed
            || alloc_changed
            || problems_changed
            || self.selected.is_some()
            || !self.forwards.is_empty()
    }

    /// Refreshes the footer's own-process stats; returns whether the text changed.
    fn sample_stats(&mut self) -> bool {
        let (cpu, mem) = self_usage();
        let now = Instant::now();
        let per_core = (cpu - self.last_sample.1) / now.duration_since(self.last_sample.0).as_secs_f64() * 100.;
        self.last_sample = (now, cpu);
        // Shown as a share of the whole machine, like btop's default; per-core (Activity Monitor/top) in the tooltip.
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        let objects: usize = CACHE.lock().unwrap().values().map(|s| s.rows.len()).sum();
        let mb = |b: u64| b as f64 / 1048576.;
        let mem = match (mem, heap_in_use()) {
            (Some(m), Some(used)) => format!("{:.0} MB ({:.0} MB in use)", mb(m), mb(used)),
            (Some(m), None) => format!("{:.0} MB", mb(m)),
            _ => "–".into(),
        };
        let text = format!("App {:.1}% CPU  ·  {mem}  ·  {objects} objects cached", per_core / cores as f64);
        self.app_stats_tip = format!(
            "CPU over the last 5s: {:.1}% of all {cores} cores (btop) = {per_core:.1}% of one core (Activity Monitor, top).\nMemory: what Activity Monitor shows, then what's actually in use. macOS keeps freed memory reserved for reuse, so the first number doesn't drop when you deselect a cluster; the second does.",
            per_core / cores as f64
        );
        let changed = text != self.app_stats;
        self.app_stats = text;
        changed
    }

    fn ctx_color(&self, ctx: &Ctx) -> u32 {
        CTX_COLORS[self.ctxs.iter().position(|c| c == ctx).unwrap_or(0) % CTX_COLORS.len()]
    }

    /// Connection state of an active context, derived from its watchers.
    fn ctx_state(ctx: &Ctx) -> (&'static str, u32) {
        let cache = CACHE.lock().unwrap();
        let slots: Vec<_> = (0..kinds().len()).filter_map(|i| cache.get(&(ctx.clone(), i))).collect();
        if slots.iter().any(|s| s.synced) {
            ("Live", GREEN)
        } else if slots.iter().any(|s| s.error.is_some()) {
            ("Error", RED)
        } else {
            ("Connecting…", FAINT)
        }
    }

    fn add_kubeconfig(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Add kubeconfig".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                this.update(cx, |this, cx| {
                    if let Err(e) = k8s::save_kubeconfig_files(&paths) {
                        this.status = format!("Could not save kubeconfig list: {e}");
                    }
                    this.reload_contexts(cx);
                    cx.notify();
                })
                .ok();
            }
        })
        .detach();
    }

    fn toggle_ctx(&mut self, ctx: Ctx, cx: &mut Context<Self>) {
        match self.active.iter().position(|c| *c == ctx) {
            Some(i) => _ = self.active.remove(i),
            None => self.active.push(ctx),
        }
        let saved: Vec<(String, String)> = self.active.iter().map(|c| (c.name.clone(), c.file.display().to_string())).collect();
        if let Err(e) = save_json("contexts.json", &saved) {
            self.status = format!("Could not save context selection: {e}");
        }
        self.sync_watchers(cx);
        cx.notify();
    }

    fn set_kind(&mut self, kind: usize, cx: &mut Context<Self>) {
        self.problems_mode = false;
        self.picked.clear();
        (self.helm_mode, self.helm_sel, self.home_mode) = (false, None, false);
        self.from_problems = false;
        self.kind = kind;
        self.watch_current();
        self.selected = None;
        self.history.clear();
        self.sync_rows();
        cx.notify();
    }

    fn apply_pending_open(&mut self, cx: &mut Context<Self>) {
        let Some(spec) = self.pending_open.take() else { return };
        if let Some(q) = spec.strip_prefix("Search:") {
            self.palette_query = Some(q.to_string());
            return cx.notify();
        }
        if let Some(rest) = spec.strip_prefix("Helm") {
            (self.helm_mode, self.home_mode) = (true, false);
            if let Some((name, tab)) = rest.strip_prefix('/').map(|r| r.split_once('#').unwrap_or((r, "overview"))) {
                if let Some(r) = self.helm_releases().into_iter().find(|r| r.name == name) {
                    self.helm_sel = Some((r.ctx, r.ns, r.name));
                    self.helm_tab = match tab {
                        "values" => HelmTab::Values,
                        "manifest" => HelmTab::Manifest,
                        "notes" => HelmTab::Notes,
                        _ => HelmTab::Overview,
                    };
                } else {
                    self.pending_open = Some(spec); // releases arrive with the Secrets watch
                }
            }
            return cx.notify();
        }
        if spec == "Overview" {
            return cx.notify(); // already the start page
        }
        if spec == "Problems" {
            (self.problems_mode, self.home_mode) = (true, false);
            return cx.notify();
        }
        let (target, tab) = spec.split_once('#').unwrap_or((&spec, "overview"));
        let (kind, name) = target.split_once('/').unwrap_or((target, ""));
        // CRD pages exist once discovery has run; keep waiting until then.
        let Some(ix) = k8s::find_kind(|k| k.name == kind) else {
            self.pending_open = Some(spec);
            return;
        };
        // Also when the kind is already selected but another page (Overview, Problems, Helm) is showing.
        if self.kind != ix || self.home_mode || self.problems_mode || self.helm_mode {
            self.set_kind(ix, cx);
            self.pending_open = Some(spec); // rows for the new kind arrive on the next tick
            return;
        }
        if tab == "pick" {
            self.compute_view();
            self.picked = self
                .view
                .iter()
                .take(3)
                .map(|&i| (self.rows[i].ctx.clone(), self.rows[i].ns.clone(), self.rows[i].name.clone()))
                .collect();
            return cx.notify();
        }
        if tab == "ns" {
            self.ns_open = true;
            cx.notify();
            return;
        }
        if tab == "picker" {
            self.picker_open = true;
            cx.notify();
            return;
        }
        let row = self.rows.iter().find(|r| name.is_empty() || r.name == name);
        if let (Some(r), false) = (row, name.is_empty()) {
            let key = (r.ctx.clone(), r.ns.clone(), r.name.clone());
            let tab = match tab {
                "yaml" => Tab::Yaml,
                "events" => Tab::Events,
                "map" => Tab::Map,
                "menu" => {
                    self.debug_menu = Some((self.kind, key.clone()));
                    return cx.notify();
                }
                "forward" => {
                    // Dev hook for screenshots: open the port-forward modal.
                    self.open_fwd_modal(r.clone(), 0, cx);
                    Tab::Overview
                }
                "forward!" => {
                    // Dev hook for screenshots: start forwarding the first port (never opens the browser).
                    if let Some(t) = self.forward_targets(&r.clone()).into_iter().next() {
                        self.open_browser = false;
                        let row = r.clone();
                        self.start_forward(&row, t, None, cx);
                    }
                    Tab::Overview
                }
                "logs" => Tab::Logs,
                _ => Tab::Overview,
            };
            self.open_tab(self.kind, key, tab, cx);
        }
        cx.notify();
    }

    /// Select an object and show a given detail tab (from the row context menu).
    fn open_tab(&mut self, kind: usize, key: Key, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        self.open(kind, key, cx);
        cx.notify();
    }

    fn save_sidebar(&mut self) {
        let state: SidebarState = [
            ("expanded".into(), self.expanded.clone()),
            ("collapsed".into(), self.collapsed.clone()),
        ]
        .into();
        if let Err(e) = save_json("sidebar.json", &state) {
            self.status = format!("Could not save sidebar layout: {e}");
        }
    }

    /// Clickable section title with a fold chevron; highlighted when folded over the open page.
    fn section_header(&self, icon_name: &str, label: &'static str, cx: &mut Context<Self>) -> AnyElement {
        let open = !self.collapsed.contains(label);
        let has_current = section_of(self.kind) == label && !self.problems_mode && !self.helm_mode && !self.home_mode;
        section(icon_name, label)
            .id(label)
            .pt_4()
            .pr_1()
            .cursor_pointer()
            .when(has_current && !open, |d| d.text_color(rgb(ACCENT)))
            .hover(|s| s.text_color(rgb(DIM)))
            .on_click(cx.listener(move |this, _, _, cx| {
                if !this.collapsed.remove(label) {
                    this.collapsed.insert(label.to_string());
                }
                this.save_sidebar();
                cx.notify();
            }))
            .child(div().flex_1())
            .child(icon(if open { "chevron-down" } else { "chevron-right" }, FAINT).size(px(12.)))
            .into_any_element()
    }

    /// CRD pages start their watchers on first view (built-ins/Flux are always watched).
    /// Also unfolds the page's sidebar group once, on arrival; after that it's yours to toggle.
    fn watch_current(&mut self) {
        let kind = kind_at(self.kind);
        let mut changed = self.collapsed.remove(section_of(self.kind));
        if kind.custom && !kind.is_flux() {
            changed |= self.expanded.insert(kind.group.to_string());
        }
        if changed {
            self.save_sidebar();
        }
        for ctx in self.active.clone() {
            if self.serves(&ctx, self.kind) {
                self.watch_kind(&ctx, self.kind);
            }
        }
    }

    /// Jump to another object (e.g. node -> pod), remembering where we came from.
    fn goto(&mut self, kind: usize, key: Key, cx: &mut Context<Self>) {
        if let Some(cur) = self.selected.clone() {
            self.history.push((self.kind, cur));
        }
        self.open(kind, key, cx);
    }

    /// One step back: the previous object if we followed a link, else the list (scrolled to that row).
    fn back(&mut self, cx: &mut Context<Self>) {
        match self.history.pop() {
            Some((kind, key)) => self.open(kind, key, cx),
            None if self.from_problems => {
                (self.selected, self.from_problems, self.problems_mode) = (None, false, true);
                cx.notify();
            }
            None => {
                if let Some((c, ns, n)) = self.selected.take()
                    && let Some(p) = self.view.iter().position(|&i| {
                        let r = &self.rows[i];
                        r.ctx == c && r.ns == ns && r.name == n
                    })
                {
                    self.list_scroll.scroll_to_item(p, ScrollStrategy::Center);
                }
                cx.notify();
            }
        }
    }

    fn open(&mut self, kind: usize, key: Key, cx: &mut Context<Self>) {
        self.problems_mode = false;
        (self.helm_mode, self.helm_sel, self.home_mode) = (false, None, false);
        if self.kind != kind {
            self.kind = kind;
            self.watch_current();
        }
        self.filter.clear();
        self.sync_rows();
        self.compute_view();
        self.select(key.clone(), cx);
        let (c, ns, n) = &key;
        if let Some(p) = self.view.iter().position(|&i| {
            let r = &self.rows[i];
            r.ctx == *c && r.ns == *ns && r.name == *n
        }) {
            self.list_scroll.scroll_to_item(p, ScrollStrategy::Center);
        }
    }

    fn selected_row(&self) -> Option<&Arc<Row>> {
        let (ctx, ns, name) = self.selected.as_ref()?;
        self.rows.iter().find(|r| r.ctx == *ctx && r.ns == *ns && r.name == *name)
    }

    /// Plain click opens the object; ⌘-click toggles it in the bulk selection; ⇧-click picks the range
    /// (in the current sort/filter order) from the last ⌘/⇧-clicked row.
    fn row_click(&mut self, key: &Key, m: gpui::Modifiers, cx: &mut Context<Self>) {
        if m.shift {
            let pos = |k: &Key| {
                self.view
                    .iter()
                    .position(|&i| self.rows[i].ctx == k.0 && self.rows[i].ns == k.1 && self.rows[i].name == k.2)
            };
            let (a, b) = (self.pick_anchor.as_ref().and_then(pos).unwrap_or(0), pos(key).unwrap_or(0));
            for &i in &self.view[a.min(b)..=a.max(b)] {
                self.picked
                    .insert((self.rows[i].ctx.clone(), self.rows[i].ns.clone(), self.rows[i].name.clone()));
            }
        } else if m.platform {
            if !self.picked.remove(key) {
                self.picked.insert(key.clone());
            }
        } else {
            self.picked.clear();
            return self.select(key.clone(), cx);
        }
        self.pick_anchor = Some(key.clone());
        cx.notify();
    }

    /// Runs an action on every picked row (no confirmation, like single actions).
    fn bulk(&mut self, action: Action, cx: &mut Context<Self>) {
        let keys: Vec<Key> = std::mem::take(&mut self.picked).into_iter().collect();
        let n = keys.len();
        for key in keys {
            self.act(action, self.kind, key, cx);
        }
        self.status = format!("{}: {n} objects…", action.label());
        cx.notify();
    }

    /// Bar shown while rows are picked: count, the kind's actions, Delete, Clear.
    fn bulk_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = kind_at(self.kind);
        let mut actions: Vec<Action> = Action::quick(kind, false).into_iter().filter(|a| *a != Action::Delete).collect();
        if kind.is_flux() {
            actions.push(Action::Resume);
        }
        actions.push(Action::Delete);
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .mx_3()
            .mb_2()
            .px_3()
            .py_1p5()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(0x34374a))
            .shadow_lg()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} selected", self.picked.len())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child("⌘-click toggle · ⇧-click range · ⌘A all"),
            )
            .child(div().flex_1())
            .children(actions.into_iter().enumerate().map(|(i, a)| {
                button(("bulk", i), a.icon(), a.label(), if a == Action::Delete { RED } else { DIM })
                    .on_click(cx.listener(move |this, _, _, cx| this.bulk(a, cx)))
            }))
            .child(button("bulk-clear", "x", "Clear", DIM).on_click(cx.listener(|this, _, _, cx| {
                this.picked.clear();
                cx.notify();
            })))
    }

    fn select(&mut self, key: Key, cx: &mut Context<Self>) {
        let entry = (kind_at(self.kind).name, key.clone());
        self.recent.retain(|e| *e != entry);
        self.recent.push_front(entry);
        self.recent.truncate(10);
        if self.selected.as_ref() != Some(&key) {
            self.selected = Some(key);
            self.revealed.clear();
            self.container = None;
        }
        match self.tab {
            Tab::Logs if has_logs(kind_at(self.kind).name) => {}
            Tab::Logs => self.tab = Tab::Overview,
            Tab::Map if has_map(kind_at(self.kind).name) => {}
            Tab::Map => self.tab = Tab::Overview,
            Tab::Yaml | Tab::Overview | Tab::Events => {}
        }
        cx.notify();
    }

    /// Creates/refreshes the editor for the selected object. Runs in render because InputState needs the window.
    fn sync_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Tab::Yaml, Some(key), Some(row)) = (self.tab, self.selected.clone(), self.selected_row().cloned()) else {
            return;
        };
        let id = (self.kind, key);
        if let Some(ed) = self.editors.get_mut(&id) {
            if !Arc::ptr_eq(&ed.base, &row) {
                let text = yaml_text(&row);
                let in_use = ed.state.read(cx).focus_handle(cx).is_focused(window);
                if text == ed.original {
                    ed.base = row;
                } else if ed.dirty(cx) || (in_use && !ed.reload_next) {
                    ed.stale = true;
                } else {
                    ed.state.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
                    (ed.base, ed.original, ed.stale, ed.reload_next) = (row, text, false, false);
                }
            }
        } else {
            let text = yaml_text(&row);
            let state = cx.new(|cx| {
                InputState::new(window, cx)
                    .code_editor("yaml")
                    .line_number(true)
                    .searchable(true)
                    .default_value(text.clone())
            });
            let ed = YamlEditor {
                state,
                base: row,
                original: text,
                stale: false,
                reload_next: false,
                saving: false,
                error: None,
            };
            self.editors.insert(id.clone(), ed);
        }
        self.editors.retain(|k, e| *k == id || e.dirty(cx));
    }

    /// Clicking the table/sidebar takes keyboard focus back from the editor (for type-to-filter).
    fn focus_root(cx: &mut Context<Self>) -> impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static {
        cx.listener(|this, _, window, _| window.focus(&this.focus))
    }

    /// Scans the live cache for broken or noteworthy objects (runs on every cache change).
    fn find_problems(&self, cache: &HashMap<(Ctx, usize), k8s::Slot>) -> Vec<Problem> {
        let now = k8s_openapi::jiff::Timestamp::now().as_second();
        let mut out = vec![];
        // All namespaces: the namespace filter applies when showing (see `shown_problems`), so changing
        // it never makes problems look new to the notifier.
        let mut push = |severity: u8, kind: usize, r: &Row, reason: String, message: String, since: i64| {
            out.push(Problem {
                severity,
                kind,
                key: (r.ctx.clone(), r.ns.clone(), r.name.clone()),
                reason,
                message,
                since,
            });
        };
        let rows = |ctx: &Ctx, kind: usize| cache.get(&(ctx.clone(), kind)).into_iter().flat_map(|s| s.rows.values());
        let s = k8s::s;
        for ctx in &self.active {
            let pods = kind_ix("Pods");
            for r in rows(ctx, pods) {
                let (ready, status) = (r.cells[0].as_str(), r.cells[1].as_str());
                let age = now - r.created;
                let statuses = k8s::arr(&r.obj, "/status/containerStatuses");
                let msg = statuses
                    .iter()
                    .find_map(|c| Some(s(c.pointer("/state/waiting")?, "/message")).filter(|m| !m.is_empty()))
                    .unwrap_or_default();
                match status {
                    "CrashLoopBackOff"
                    | "ImagePullBackOff"
                    | "ErrImagePull"
                    | "Error"
                    | "OOMKilled"
                    | "Failed"
                    | "Evicted"
                    | "CreateContainerConfigError"
                    | "CreateContainerError"
                    | "InvalidImageName"
                    | "Unknown" => push(0, pods, r, status.into(), msg, r.created),
                    "Pending" | "ContainerCreating" | "PodInitializing" if age > 300 => {
                        push(1, pods, r, format!("{status} for {}", k8s::age(r.created)), msg, r.created)
                    }
                    "Running" if age > 300 && ready.split_once('/').is_some_and(|(a, b)| a != b) => {
                        push(1, pods, r, format!("Not ready ({ready})"), msg, r.created)
                    }
                    _ => {}
                }
                // Restarted within the last hour (and not already listed as broken).
                if !matches!(status, "CrashLoopBackOff" | "Error" | "OOMKilled") {
                    for c in statuses {
                        let Some(t) = c.pointer("/lastState/terminated") else { continue };
                        let Some(at) = k8s::ts(t, "/finishedAt").filter(|at| now - at < 3600) else {
                            continue;
                        };
                        let why = format!("{} (exit {})", s(t, "/reason"), s(t, "/exitCode"));
                        push(
                            1,
                            pods,
                            r,
                            format!("Restarted {} ago", k8s::age(at)),
                            format!("{}: {why}, {} restarts total", s(c, "/name"), c["restartCount"]),
                            at,
                        );
                    }
                }
            }
            let nodes = kind_ix("Nodes");
            for r in rows(ctx, nodes) {
                let conds = k8s::arr(&r.obj, "/status/conditions");
                if r.cells[0] != "Ready" {
                    let m = conds
                        .iter()
                        .find(|c| c["type"] == "Ready")
                        .map(|c| s(c, "/message"))
                        .unwrap_or_default();
                    push(0, nodes, r, "NotReady".into(), m, r.created);
                }
                for c in conds
                    .iter()
                    .filter(|c| c["status"] == "True" && c["type"].as_str().is_some_and(|t| t.ends_with("Pressure")))
                {
                    push(
                        1,
                        nodes,
                        r,
                        s(c, "/type"),
                        s(c, "/message"),
                        k8s::ts(c, "/lastTransitionTime").unwrap_or(0),
                    );
                }
                if r.obj.pointer("/spec/unschedulable") == Some(&Value::Bool(true)) {
                    push(
                        2,
                        nodes,
                        r,
                        "Cordoned".into(),
                        "Unschedulable: no new pods are placed here".into(),
                        r.created,
                    );
                }
            }
            for kind in ["Deployments", "StatefulSets"] {
                let k = kind_ix(kind);
                for r in rows(ctx, k) {
                    let want = r.obj.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1);
                    let ready = r.obj.pointer("/status/readyReplicas").and_then(Value::as_i64).unwrap_or(0);
                    let unavailable = k8s::arr(&r.obj, "/status/conditions")
                        .iter()
                        .find(|c| c["type"] == "Available" && c["status"] == "False");
                    if let Some(c) = unavailable {
                        push(
                            0,
                            k,
                            r,
                            "Unavailable".into(),
                            s(c, "/message"),
                            k8s::ts(c, "/lastTransitionTime").unwrap_or(0),
                        );
                    } else if ready < want && now - r.created > 300 {
                        push(1, k, r, format!("{ready}/{want} ready"), String::new(), r.created);
                    }
                }
            }
            let ds = kind_ix("DaemonSets");
            for r in rows(ctx, ds) {
                if r.cells[1] != r.cells[0] {
                    push(1, ds, r, format!("{}/{} ready", r.cells[1], r.cells[0]), String::new(), r.created);
                }
            }
            let jobs = kind_ix("Jobs");
            for r in rows(ctx, jobs) {
                if let Some(c) = k8s::arr(&r.obj, "/status/conditions")
                    .iter()
                    .find(|c| c["type"] == "Failed" && c["status"] == "True")
                {
                    push(
                        0,
                        jobs,
                        r,
                        format!("Failed: {}", s(c, "/reason")),
                        s(c, "/message"),
                        k8s::ts(c, "/lastTransitionTime").unwrap_or(r.created),
                    );
                }
            }
            let pvcs = kind_ix("PVCs");
            for r in rows(ctx, pvcs) {
                match r.cells[0].as_str() {
                    "Lost" => push(0, pvcs, r, "Lost".into(), String::new(), r.created),
                    "Pending" if now - r.created > 300 => push(1, pvcs, r, "Pending".into(), String::new(), r.created),
                    _ => {}
                }
            }
            // Flux: not Ready = error, suspended = info.
            for ix in self
                .available
                .get(ctx)
                .into_iter()
                .flatten()
                .copied()
                .filter(|&ix| kind_at(ix).is_flux())
            {
                for r in rows(ctx, ix) {
                    if suspended(&r.obj) {
                        push(2, ix, r, "Suspended".into(), "Reconciliation paused".into(), r.created);
                        continue;
                    }
                    if let Some(c) = k8s::arr(&r.obj, "/status/conditions")
                        .iter()
                        .find(|c| c["type"] == "Ready" && c["status"] == "False")
                    {
                        push(
                            0,
                            ix,
                            r,
                            s(c, "/reason"),
                            s(c, "/message"),
                            k8s::ts(c, "/lastTransitionTime").unwrap_or(r.created),
                        );
                    }
                }
            }
            // Warning events from the last 30 minutes, one entry per object (latest wins, counts summed).
            let mut warned: HashMap<(String, String, String), (Arc<Row>, i64)> = HashMap::new();
            for e in rows(ctx, kind_ix("Events")) {
                if s(&e.obj, "/type") != "Warning" || now - e.created > 1800 {
                    continue;
                }
                let io = &e.obj["involvedObject"];
                let id = (s(io, "/kind"), s(io, "/namespace"), s(io, "/name"));
                let count = e.obj["count"].as_i64().unwrap_or(1);
                let entry = warned.entry(id).or_insert((e.clone(), 0));
                if e.created > entry.0.created {
                    entry.0 = e.clone();
                }
                entry.1 += count;
            }
            let events = kind_ix("Events");
            for ((kind, ns, name), (e, count)) in warned {
                // Point at the object itself when we know its kind; otherwise at the event.
                let target = k8s::find_kind(|k| k.kind == kind && !k.custom).or_else(|| k8s::find_kind(|k| k.kind == kind));
                let (kix, row) = match target.and_then(|ix| rows(ctx, ix).find(|r| r.ns == ns && r.name == name).map(|r| (ix, r.clone()))) {
                    Some(found) => found,
                    None => (events, e.clone()),
                };
                let times = if count > 1 { format!(" ×{count}") } else { String::new() };
                push(
                    1,
                    kix,
                    &row,
                    format!("{}{times}", s(&e.obj, "/reason")),
                    s(&e.obj, "/message"),
                    e.created,
                );
            }
        }
        out.sort_by(|a, b| a.severity.cmp(&b.severity).then(b.since.cmp(&a.since)));
        out
    }

    /// Overview page: per selected cluster, version, health counts, capacity bars, Flux and Helm.
    fn home_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut body = div()
            .id("home")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_4();
        if self.active.is_empty() {
            return body.child(
                div()
                    .pt_16()
                    .flex()
                    .justify_center()
                    .text_color(rgb(FAINT))
                    .child("Pick a context from the switcher (top left, ⌘K)"),
            );
        }
        let helm = self.helm_revisions();
        for (ci, ctx) in self.active.iter().enumerate() {
            body = body.child(self.cluster_card(ci, ctx, &helm, cx));
        }
        body
    }

    fn cluster_card(
        &self,
        ci: usize,
        ctx: &Ctx,
        helm: &HashMap<(Ctx, String, String), Vec<(i64, String, Arc<Row>)>>,
        cx: &mut Context<Self>,
    ) -> Div {
        let cache = CACHE.lock().unwrap();
        let rows = |kind: &str| -> Vec<Arc<Row>> {
            cache
                .get(&(ctx.clone(), kind_ix(kind)))
                .map(|s| s.rows.values().cloned().collect())
                .unwrap_or_default()
        };
        let (nodes, pods, deploys, namespaces) = (rows("Nodes"), rows("Pods"), rows("Deployments"), rows("Namespaces"));
        // Most common kubelet version (mixed versions show during upgrades).
        let mut versions: HashMap<String, usize> = HashMap::new();
        for n in &nodes {
            *versions.entry(k8s::s(&n.obj, "/status/nodeInfo/kubeletVersion")).or_default() += 1;
        }
        let version = versions.into_iter().max_by_key(|(_, n)| *n).map(|(v, _)| v).unwrap_or_default();
        let nodes_ready = nodes.iter().filter(|n| n.cells.first().is_some_and(|c| c == "Ready")).count();
        let running = pods.iter().filter(|p| k8s::s(&p.obj, "/status/phase") == "Running").count();
        let failing = self
            .problems
            .iter()
            .filter(|p| p.key.0 == *ctx && p.kind == kind_ix("Pods") && p.severity == 0)
            .count();
        let deploys_ready = deploys
            .iter()
            .filter(|d| {
                d.obj.pointer("/status/readyReplicas").and_then(Value::as_i64).unwrap_or(0)
                    >= d.obj.pointer("/spec/replicas").and_then(Value::as_i64).unwrap_or(1)
            })
            .count();
        // Flux: Kustomizations + HelmReleases (the things you deploy with).
        let (mut flux_total, mut flux_ready, mut flux_suspended) = (0, 0, 0);
        for k in ["Kustomization", "HelmRelease"] {
            let Some(ix) = k8s::find_kind(|x| x.is_flux() && x.kind == k) else {
                continue;
            };
            for r in cache.get(&(ctx.clone(), ix)).into_iter().flat_map(|s| s.rows.values()) {
                flux_total += 1;
                if suspended(&r.obj) {
                    flux_suspended += 1;
                } else if k8s::arr(&r.obj, "/status/conditions")
                    .iter()
                    .any(|c| c["type"] == "Ready" && c["status"] == "True")
                {
                    flux_ready += 1;
                }
            }
        }
        drop(cache);
        let releases = helm.keys().filter(|(c, ..)| c == ctx).count();
        let failed_releases = helm
            .iter()
            .filter(|((c, ..), revs)| c == ctx && revs.first().is_some_and(|r| r.1 == "failed"))
            .count();
        // Capacity: sum over nodes of allocatable, used (metrics), requests/limits (scheduled pods).
        let mut cap = Usage::default();
        for n in &nodes {
            cap += capacity(&n.obj).0;
        }
        let used = self.metrics.get(ctx).map(|m| {
            let mut u = Usage::default();
            m.nodes.values().for_each(|n| u += *n);
            u
        });
        let (mut req, mut lim) = (Usage::default(), Usage::default());
        for ((c, _), a) in &self.alloc {
            if c == ctx {
                req += a.req;
                lim += a.lim;
            }
        }
        let (errors, warnings) = (
            self.problems.iter().filter(|p| p.key.0 == *ctx && p.severity == 0).count(),
            self.problems.iter().filter(|p| p.key.0 == *ctx && p.severity == 1).count(),
        );
        let (state, state_color) = Self::ctx_state(ctx);

        let tile = |id: &str, icon_name: &'static str, label: &'static str, value: String, sub: String, color: u32, kind: Option<usize>| {
            div()
                .id(SharedString::from(format!("{id}-{ci}")))
                .flex_1()
                .min_w(px(130.))
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .rounded_md()
                .bg(rgb(PANEL))
                .border_1()
                .border_color(rgb(BORDER))
                .when(kind.is_some(), |d| d.cursor_pointer().hover(|s| s.border_color(rgb(0x3a3d50))))
                .when_some(kind, |d, k| d.on_click(cx.listener(move |this, _, _, cx| this.set_kind(k, cx))))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_xs()
                        .text_color(rgb(FAINT))
                        .child(icon(icon_name, FAINT).size(px(12.)))
                        .child(label),
                )
                .child(
                    div()
                        .text_xl()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(color))
                        .child(value),
                )
                .child(div().text_xs().text_color(rgb(FAINT)).child(sub))
        };
        let health = |ok: usize, total: usize| {
            if total == 0 {
                DIM
            } else if ok == total {
                TEXT
            } else {
                YELLOW
            }
        };
        let flux_ix = k8s::find_kind(|x| x.is_flux() && x.kind == "Kustomization");
        div()
            .flex()
            .flex_col()
            .gap_4()
            .p_4()
            .rounded_xl()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(self.ctx_avatar(ctx))
                    .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(ctx.name.clone()))
                    .when(!version.is_empty(), |d| {
                        d.child(div().text_sm().text_color(rgb(FAINT)).child(format!("Kubernetes {version}")))
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(("home-problems", ci))
                            .flex()
                            .items_center()
                            .gap_3()
                            .text_sm()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                (this.problems_mode, this.home_mode) = (true, false);
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .text_color(rgb(if errors > 0 { RED } else { FAINT }))
                                    .child(format!("{errors} errors")),
                            )
                            .child(
                                div()
                                    .text_color(rgb(if warnings > 0 { YELLOW } else { FAINT }))
                                    .child(format!("{warnings} warnings")),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .text_xs()
                            .text_color(rgb(state_color))
                            .child(div().size(px(6.)).rounded_full().bg(rgb(state_color)))
                            .child(state),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_3()
                    .child(tile(
                        "t-nodes",
                        "cpu",
                        "Nodes",
                        format!("{nodes_ready}/{}", nodes.len()),
                        "ready".into(),
                        health(nodes_ready, nodes.len()),
                        Some(kind_ix("Nodes")),
                    ))
                    .child(tile(
                        "t-pods",
                        "box",
                        "Pods",
                        running.to_string(),
                        if failing > 0 {
                            format!("{failing} failing · {} total", pods.len())
                        } else {
                            format!("running · {} total", pods.len())
                        },
                        if failing > 0 { RED } else { TEXT },
                        Some(kind_ix("Pods")),
                    ))
                    .child(tile(
                        "t-deploys",
                        "rocket",
                        "Deployments",
                        format!("{deploys_ready}/{}", deploys.len()),
                        "fully ready".into(),
                        health(deploys_ready, deploys.len()),
                        Some(kind_ix("Deployments")),
                    ))
                    .child(tile(
                        "t-ns",
                        "folder",
                        "Namespaces",
                        namespaces.len().to_string(),
                        String::new(),
                        TEXT,
                        Some(kind_ix("Namespaces")),
                    ))
                    .when(flux_total > 0, |d| {
                        let sub = if flux_suspended > 0 {
                            format!("ready · {flux_suspended} suspended")
                        } else {
                            "Kustomizations + HelmReleases ready".into()
                        };
                        d.child(tile(
                            "t-flux",
                            "refresh-cw",
                            "Flux",
                            format!("{flux_ready}/{}", flux_total - flux_suspended),
                            sub,
                            health(flux_ready, flux_total - flux_suspended),
                            flux_ix,
                        ))
                    })
                    .child(tile(
                        "t-helm",
                        "ship",
                        "Helm releases",
                        releases.to_string(),
                        if failed_releases > 0 {
                            format!("{failed_releases} failed")
                        } else {
                            String::new()
                        },
                        if failed_releases > 0 { RED } else { TEXT },
                        None,
                    )),
            )
            .child(
                div()
                    .flex()
                    .gap_6()
                    .child(div().flex_1().min_w_0().child(resource_bar(
                        "CPU",
                        format!("of {} cores allocatable", fmt_cpu(cap.cpu)),
                        used.map(|u| u.cpu),
                        req.cpu,
                        lim.cpu,
                        cap.cpu,
                        fmt_cpu,
                    )))
                    .child(div().flex_1().min_w_0().child(resource_bar(
                        "Memory",
                        format!("of {} allocatable", fmt_mem(cap.mem)),
                        used.map(|u| u.mem),
                        req.mem,
                        lim.mem,
                        cap.mem,
                        fmt_mem,
                    ))),
            )
    }

    fn app_map(&self, r: &Row) -> Vec<(&'static str, &'static str, Vec<MapNode>)> {
        app_map(self.kind, r)
    }

    fn map_view(&self, r: &Arc<Row>, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        const MAX: usize = 12;
        let cols = self.app_map(r);
        let me = (self.kind, r.name.clone());
        let n_cols = cols.len();
        // Columns share the page width (arrows ~24px each), between 170 and 260px.
        let col_w = ((self.detail_w - 32. - n_cols.saturating_sub(1) as f32 * 32.) / n_cols.max(1) as f32).clamp(170., 260.);
        div()
            .id("map")
            .flex_1()
            .min_w_0()
            .overflow_scroll()
            .px_4()
            .py_4()
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .children(cols.into_iter().enumerate().flat_map(|(ci, (title, icon_name, nodes))| {
                        let total = nodes.len();
                        let column = div()
                            .flex()
                            .flex_col()
                            .flex_none()
                            .w(px(col_w))
                            .gap_2()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(rgb(FAINT))
                                    .child(icon(icon_name, FAINT).size(px(12.)))
                                    .child(format!("{} ({total})", title.to_uppercase())),
                            )
                            .children(nodes.into_iter().take(MAX).enumerate().map(|(ni, n)| {
                                let current = (n.kind, n.name.clone()) == me;
                                let target = (n.kind, (r.ctx.clone(), r.ns.clone(), n.name.clone()));
                                div()
                                    .id(("map-node", ci * 1000 + ni))
                                    .flex()
                                    .gap_2()
                                    .p_2()
                                    .rounded_md()
                                    .bg(rgb(SURFACE))
                                    .border_1()
                                    .border_color(rgb(if current {
                                        ACCENT
                                    } else if n.missing {
                                        RED
                                    } else {
                                        BORDER
                                    }))
                                    .when(current, |d| d.bg(rgb(SELECT)))
                                    .when(!n.missing && !current, |d| {
                                        d.cursor_pointer().hover(|s| s.border_color(rgb(0x3a3d50))).on_click(cx.listener(
                                            move |this, _, _, cx| {
                                                this.tab = Tab::Map;
                                                this.goto(target.0, target.1.clone(), cx)
                                            },
                                        ))
                                    })
                                    .child(div().mt(px(5.)).flex_none().size(px(7.)).rounded_full().bg(rgb(n.color)))
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .child(div().text_sm().text_color(rgb(if n.missing { RED } else { TEXT })).child(fit_text(
                                                window,
                                                cx,
                                                n.name.clone(),
                                                col_w - 45.,
                                            )))
                                            .child(div().text_xs().text_color(rgb(FAINT)).child(fit_text(
                                                window,
                                                cx,
                                                n.sub.clone(),
                                                (col_w - 45.) * 14. / 12.,
                                            ))),
                                    )
                            }))
                            .when(total > MAX, |d| {
                                d.child(
                                    div()
                                        .pl_2()
                                        .text_xs()
                                        .text_color(rgb(FAINT))
                                        .child(format!("+{} more", total - MAX)),
                                )
                            })
                            .into_any_element();
                        let arrow = (ci + 1 < n_cols).then(|| {
                            div()
                                .flex_none()
                                .pt(px(34.))
                                .child(icon("chevron-right", FAINT).size(px(16.)))
                                .into_any_element()
                        });
                        std::iter::once(column).chain(arrow)
                    })),
            )
            .child(
                div()
                    .pt_6()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child("Same namespace only. Red boxes are referenced but don't exist."),
            )
    }

    /// Problems in the namespaces the filter allows.
    fn shown_problems(&self) -> impl Iterator<Item = &Problem> {
        self.problems
            .iter()
            .filter(|p| self.ns_filter.is_empty() || p.key.1.is_empty() || self.ns_filter.contains(&p.key.1))
    }

    /// The Problems page: errors, warnings and info across the selected clusters; click to open.
    fn problems_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(String::from).collect();
        let visible: Vec<&Problem> = self
            .shown_problems()
            .filter(|p| {
                let line = format!("{} {} {} {} {}", kind_at(p.kind).kind, p.key.1, p.key.2, p.reason, p.message).to_lowercase();
                terms.iter().all(|t| line.contains(t))
            })
            .collect();
        let many = self.active.len() > 1;
        // min_w_0: without it the page grows to its longest message instead of wrapping.
        let mut body = div()
            .id("problems")
            .flex_1()
            .min_w_0()
            .overflow_y_scroll()
            .px_4()
            .py_4()
            .flex()
            .flex_col()
            .gap_6();
        body = body.child(
            div()
                .id("notify-toggle")
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(rgb(DIM))
                .cursor_pointer()
                .hover(|s| s.text_color(rgb(TEXT)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.notify = !this.notify;
                    this.save_settings();
                    cx.notify();
                }))
                .child(checkbox(self.notify))
                .child("Notify me about new errors (macOS notification, also while the app is in the background)"),
        );
        if visible.is_empty() {
            return body.child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .pt_8()
                    .justify_center()
                    .text_color(rgb(DIM))
                    .child(icon("circle-check", GREEN).size(px(18.)))
                    .child(if self.problems.is_empty() {
                        "No problems in the selected clusters."
                    } else {
                        "No problems match the filter."
                    }),
            );
        }
        for (sev, title, color) in [(0u8, "ERRORS", RED), (1, "WARNINGS", YELLOW), (2, "INFO", FAINT)] {
            let group: Vec<&&Problem> = visible.iter().filter(|p| p.severity == sev).collect();
            if group.is_empty() {
                continue;
            }
            body = body.child(
                card(format!("{title} ({})", group.len())).children(group.into_iter().enumerate().map(|(i, p)| {
                    let k = kind_at(p.kind);
                    let (kind_ix_, key) = (p.kind, p.key.clone());
                    div()
                        .id((title, i))
                        .flex()
                        .gap_3()
                        .px_2()
                        .py_1p5()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(HOVER)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.history.clear();
                            this.from_problems = true;
                            this.tab = Tab::Overview;
                            this.open(kind_ix_, key.clone(), cx);
                        }))
                        .child(div().mt(px(6.)).flex_none().size(px(7.)).rounded_full().bg(rgb(color)))
                        .child(icon(k.icon, FAINT).mt(px(2.)))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .flex()
                                        .items_baseline()
                                        .gap_2()
                                        .child(div().font_weight(FontWeight::MEDIUM).child(p.key.2.clone()))
                                        .child(div().text_xs().text_color(rgb(FAINT)).child(format!(
                                            "{}{}{}",
                                            k.kind,
                                            if p.key.1.is_empty() {
                                                String::new()
                                            } else {
                                                format!(" · {}", p.key.1)
                                            },
                                            if many { format!(" · {}", p.key.0.name) } else { String::new() }
                                        )))
                                        .child(div().flex_1())
                                        .when(p.since > 0, |d| {
                                            d.child(
                                                div()
                                                    .flex_none()
                                                    .text_xs()
                                                    .text_color(rgb(FAINT))
                                                    .child(format!("{} ago", k8s::age(p.since))),
                                            )
                                        }),
                                )
                                .child(div().text_sm().text_color(rgb(color)).child(p.reason.clone()))
                                .when(!p.message.is_empty(), |d| {
                                    d.child(div().text_xs().text_color(rgb(DIM)).child(brief(&p.message)))
                                }),
                        )
                })),
            );
        }
        body
    }

    fn cached_row(&self, kind: usize, key: &Key) -> Option<Arc<Row>> {
        CACHE
            .lock()
            .unwrap()
            .get(&(key.0.clone(), kind))?
            .rows
            .get(&(key.1.clone(), key.2.clone()))
            .cloned()
    }

    /// Objects a Kustomization/HelmRelease manages: Flux's `status.inventory`, or for HelmReleases
    /// without one, cached objects labelled by helm-controller. (kind label, kind index if known, ns, name)
    fn managed_objects(&self, r: &Row) -> Vec<(String, Option<usize>, String, String)> {
        let entries = k8s::arr(&r.obj, "/status/inventory/entries");
        if !entries.is_empty() {
            return entries
                .iter()
                .filter_map(|e| {
                    // id = <namespace>_<name>_<group>_<Kind> (none of these can contain '_')
                    let id = e["id"].as_str()?;
                    let mut parts = id.splitn(4, '_');
                    let (ns, name, group, kind) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
                    let ix = k8s::find_kind(|k| k.kind == kind && k.group == group);
                    Some((kind.to_string(), ix, ns.to_string(), name.to_string()))
                })
                .collect();
        }
        if kind_at(self.kind).kind != "HelmRelease" {
            return vec![];
        }
        let cache = CACHE.lock().unwrap();
        let mut out = vec![];
        for ((ctx, ix), slot) in cache.iter() {
            if *ctx != r.ctx {
                continue;
            }
            for o in slot.rows.values() {
                let labels = &o.obj["metadata"]["labels"];
                if labels["helm.toolkit.fluxcd.io/name"] == r.name.as_str() && labels["helm.toolkit.fluxcd.io/namespace"] == r.ns.as_str() {
                    out.push((kind_at(*ix).kind.to_string(), Some(*ix), o.ns.clone(), o.name.clone()));
                }
            }
        }
        out.sort_by(|a, b| (&a.0, &a.2, &a.3).cmp(&(&b.0, &b.2, &b.3)));
        out
    }

    /// "Managed objects" card: grouped by kind; objects we have cached are links.
    fn inventory_card(&self, r: &Row, cx: &mut Context<Self>) -> Option<Div> {
        let objects = self.managed_objects(r);
        if objects.is_empty() {
            return None;
        }
        let mut groups: std::collections::BTreeMap<String, Vec<(Option<usize>, String, String)>> = Default::default();
        for (kind, ix, ns, name) in objects.iter().cloned() {
            groups.entry(kind).or_default().push((ix, ns, name));
        }
        let mut n = 0usize;
        Some(
            card(format!("Managed objects ({})", objects.len())).children(groups.into_iter().map(|(kind, items)| {
                let icon_name = items.iter().find_map(|(ix, ..)| ix.map(|i| kind_at(i).icon)).unwrap_or("puzzle");
                div()
                    .flex()
                    .flex_col()
                    .gap_px()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .text_color(rgb(FAINT))
                            .child(icon(icon_name, FAINT).size(px(12.)))
                            .child(format!("{kind} ({})", items.len())),
                    )
                    .children(items.into_iter().map(|(ix, ns, name)| {
                        n += 1;
                        let link = ix.and_then(|ix| self.link(&r.ctx, ix, &ns, &name));
                        let label = if ns.is_empty() || ns == r.ns {
                            name.clone()
                        } else {
                            format!("{ns} / {name}")
                        };
                        div()
                            .id(("managed", n))
                            .pl_5()
                            .py_0p5()
                            .text_sm()
                            .truncate()
                            .map(|d| match link {
                                Some(Link::Object(kind, key)) => d
                                    .text_color(rgb(ACCENT))
                                    .cursor_pointer()
                                    .hover(|s| s.underline())
                                    .on_click(cx.listener(move |this, _, _, cx| this.goto(kind, key.clone(), cx))),
                                _ => d.text_color(rgb(DIM)),
                            })
                            .child(label)
                    }))
            })),
        )
    }

    /// Its search field had focus; the field is dropped, so focus must be handed back explicitly
    /// (for one frame the dead field still counts as focused, so "refocus when nothing is" misses it).
    fn close_palette(&mut self) {
        self.palette = None;
        self.refocus = true;
    }

    fn open_palette(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search pods, deployments, services… or a page"));
        input.update(cx, |s, cx| s.focus(window, cx));
        cx.subscribe(&input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.palette_sel = 0;
                cx.notify();
            }
        })
        .detach();
        self.palette = Some(input);
        self.palette_sel = 0;
        cx.notify();
    }

    /// Search results: pages whose name matches, then cached objects (all terms must match), names first.
    /// (score, kind, key or None for a page)
    fn palette_results(&self, cx: &App) -> Vec<(u8, usize, Option<Key>)> {
        let query = self
            .palette
            .as_ref()
            .map(|i| i.read(cx).value().trim().to_lowercase())
            .unwrap_or_default();
        if query.is_empty() {
            // Recently opened objects that still exist in a selected cluster.
            return self
                .recent
                .iter()
                .filter(|(_, k)| self.active.contains(&k.0))
                .filter_map(|(kind, k)| {
                    let ix = k8s::find_kind(|x| x.name == *kind)?;
                    self.cached_row(ix, k)?;
                    Some((0, ix, Some(k.clone())))
                })
                .collect();
        }
        let terms: Vec<&str> = query.split_whitespace().collect();
        // Ranking: exact name 0, page (kind) 1, name prefix 2, name contains 3, other fields 4.
        // Pages capped at 3 so CRD-heavy clusters don't bury objects.
        let mut out: Vec<(u8, usize, Option<Key>)> = kinds()
            .iter()
            .enumerate()
            .filter(|(i, k)| {
                (*i < k8s::BUILTIN_COUNT || self.available.values().any(|a| a.contains(i))) && k.name.to_lowercase().starts_with(&query)
            })
            .map(|(i, _)| (1, i, None))
            .take(3)
            .collect();
        let events = kind_ix("Events");
        let cache = CACHE.lock().unwrap();
        for ((ctx, ix), slot) in cache.iter() {
            if *ix == events || !self.active.contains(ctx) {
                continue;
            }
            let kind = kind_at(*ix);
            for r in slot.rows.values() {
                if kind.namespaced && !self.ns_filter.is_empty() && !self.ns_filter.contains(&r.ns) {
                    continue;
                }
                let name = r.name.to_lowercase();
                let hay = format!(
                    "{name} {} {} {}",
                    r.ns.to_lowercase(),
                    kind.kind.to_lowercase(),
                    ctx.name.to_lowercase()
                );
                if !terms.iter().all(|t| hay.contains(t)) {
                    continue;
                }
                let score = match () {
                    _ if name == terms[0] => 0,
                    _ if name.starts_with(terms[0]) => 2,
                    _ if name.contains(terms[0]) => 3,
                    _ => 4,
                };
                out.push((score, *ix, Some((ctx.clone(), r.ns.clone(), r.name.clone()))));
            }
        }
        // Fully deterministic order (the cache is a HashMap): otherwise results reshuffle on every
        // cluster update and the highlighted row jumps under the arrow keys.
        let order = |h: &(u8, usize, Option<Key>)| {
            let (ctx, ns, name) = h.2.clone().map(|(c, ns, n)| (c.name, ns, n)).unwrap_or_default();
            (h.0, name, kind_at(h.1).name, ns, ctx)
        };
        out.sort_by_cached_key(order);
        out.truncate(50);
        out
    }

    fn palette_open_result(&mut self, (_, kind, key): (u8, usize, Option<Key>), cx: &mut Context<Self>) {
        self.close_palette();
        match key {
            Some(key) => {
                self.history.clear();
                self.from_problems = false;
                self.tab = Tab::Overview;
                self.open(kind, key, cx);
            }
            None => self.set_kind(kind, cx),
        }
    }

    fn palette_view(&self, input: &Entity<InputState>, cx: &mut Context<Self>) -> impl IntoElement {
        let results = self.palette_results(cx);
        let sel = self.palette_sel.min(results.len().saturating_sub(1));
        let many = self.active.len() > 1;
        let query_empty = input.read(cx).value().trim().is_empty();
        div()
            .id("palette")
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                let n = this.palette_results(cx).len();
                // The RECENT header (empty query) sits before the first result.
                let off = usize::from(this.palette.as_ref().is_some_and(|i| i.read(cx).value().trim().is_empty()));
                match ev.keystroke.key.as_str() {
                    "escape" => this.close_palette(),
                    "down" => {
                        this.palette_sel = (this.palette_sel + 1).min(n.saturating_sub(1));
                        this.palette_scroll.scroll_to_item(this.palette_sel + off);
                    }
                    "up" => {
                        this.palette_sel = this.palette_sel.saturating_sub(1);
                        this.palette_scroll.scroll_to_item(this.palette_sel + off);
                    }
                    "enter" => {
                        if let Some(hit) = this.palette_results(cx).into_iter().nth(this.palette_sel.min(n.saturating_sub(1))) {
                            this.palette_open_result(hit, cx);
                        }
                    }
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .w(px(640.))
            .max_h(px(520.))
            .flex()
            .flex_col()
            .rounded_xl()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(0x34374a))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(icon("search", FAINT))
                    .child(div().flex_1().child(Input::new(input).appearance(false))),
            )
            .child(
                div()
                    .id("palette-results")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.palette_scroll)
                    .p_1p5()
                    .flex()
                    .flex_col()
                    .gap_px()
                    .when(query_empty && results.is_empty(), |d| {
                        d.child(
                            div().px_2().py_3().text_sm().text_color(rgb(FAINT)).child(
                                "Type to search everything in the selected clusters. Several words narrow it down, e.g. “api prod”.",
                            ),
                        )
                    })
                    .when(query_empty && !results.is_empty(), |d| {
                        d.child(
                            div()
                                .px_2()
                                .pt_1()
                                .pb_1()
                                .text_xs()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(FAINT))
                                .child("RECENT"),
                        )
                    })
                    .when(!query_empty && results.is_empty(), |d| {
                        d.child(div().px_2().py_3().text_sm().text_color(rgb(FAINT)).child("No matches."))
                    })
                    .children(results.into_iter().enumerate().map(|(i, hit)| {
                        let k = kind_at(hit.1);
                        let on = i == sel;
                        let (title, sub) = match &hit.2 {
                            Some((ctx, ns, name)) => (
                                name.clone(),
                                format!(
                                    "{}{}{}",
                                    k.kind,
                                    if ns.is_empty() { String::new() } else { format!(" · {ns}") },
                                    if many { format!(" · {}", ctx.name) } else { String::new() }
                                ),
                            ),
                            None => (k.name.to_string(), "Page".to_string()),
                        };
                        div()
                            .id(("hit", i))
                            .flex()
                            .items_center()
                            .gap_2p5()
                            .px_2()
                            .py_1p5()
                            .rounded_md()
                            .cursor_pointer()
                            .when(on, |d| d.bg(rgb(SELECT)))
                            .when(!on, |d| d.hover(|s| s.bg(rgb(HOVER))))
                            .on_click(cx.listener(move |this, _, _, cx| this.palette_open_result(hit.clone(), cx)))
                            .child(icon(k.icon, if on { ACCENT } else { FAINT }))
                            .child(div().flex_1().min_w_0().truncate().text_color(rgb(TEXT)).child(title))
                            .child(div().flex_none().text_xs().text_color(rgb(FAINT)).child(sub))
                    })),
            )
            .when(!self.ns_filter.is_empty(), |d| {
                d.child(
                    div()
                        .px_3()
                        .py_1p5()
                        .border_t_1()
                        .border_color(rgb(BORDER))
                        .text_xs()
                        .text_color(rgb(FAINT))
                        .child("Namespace filter is on: only those namespaces are searched."),
                )
            })
    }

    fn save_settings(&mut self) {
        let settings: HashMap<String, bool> = [
            ("open_browser".into(), self.open_browser),
            ("notify".into(), self.notify),
            ("log_wrap".into(), self.log_wrap),
        ]
        .into();
        if let Err(e) = save_json("settings.json", &settings) {
            self.status = format!("Could not save settings: {e}");
        }
    }

    /// Posts a notification for errors that weren't there before. A context's first minute is baseline,
    /// so selecting a cluster with existing errors doesn't spam you.
    fn notify_new_problems(&mut self) {
        let errors: Vec<&Problem> = self.problems.iter().filter(|p| p.severity == 0).collect();
        let fresh = new_problems(&self.notified, &errors);
        self.notified = errors.iter().map(|p| problem_id(p)).collect();
        let settled = |p: &Problem| self.ctx_since.get(&p.key.0).is_some_and(|t| t.elapsed() > Duration::from_secs(60));
        let fresh: Vec<&Problem> = fresh.into_iter().filter(|p| settled(p)).collect();
        if !self.notify || fresh.is_empty() {
            return;
        }
        let (title, subtitle, body) = match fresh.as_slice() {
            [p] => {
                let k = kind_at(p.kind).kind;
                let ns = if p.key.1.is_empty() {
                    String::new()
                } else {
                    format!("{}/", p.key.1)
                };
                (
                    format!("{}: {}", p.key.0.name, p.reason),
                    format!("{k} {ns}{}", p.key.2),
                    brief(&p.message),
                )
            }
            many => {
                let ctxs: std::collections::BTreeSet<&str> = many.iter().map(|p| p.key.0.name.as_str()).collect();
                let names: Vec<String> = many.iter().take(3).map(|p| format!("{} ({})", p.key.2, p.reason)).collect();
                (
                    format!("{} new errors", many.len()),
                    ctxs.into_iter().collect::<Vec<_>>().join(", "),
                    names.join(", "),
                )
            }
        };
        notify_macos(&title, &subtitle, &body);
    }

    /// Every revision Secret per release, from the cached Secrets of the selected clusters.
    fn helm_revisions(&self) -> HashMap<(Ctx, String, String), Vec<(i64, String, Arc<Row>)>> {
        let cache = CACHE.lock().unwrap();
        let mut out: HashMap<(Ctx, String, String), Vec<(i64, String, Arc<Row>)>> = HashMap::new();
        for ctx in &self.active {
            for r in cache
                .get(&(ctx.clone(), kind_ix("Secrets")))
                .into_iter()
                .flat_map(|s| s.rows.values())
            {
                if r.obj["type"] != "helm.sh/release.v1" {
                    continue;
                }
                let labels = &r.obj["metadata"]["labels"];
                let (Some(name), Some(rev)) = (labels["name"].as_str(), labels["version"].as_str().and_then(|v| v.parse().ok())) else {
                    continue;
                };
                let status = labels["status"].as_str().unwrap_or("unknown").to_string();
                out.entry((ctx.clone(), r.ns.clone(), name.to_string()))
                    .or_default()
                    .push((rev, status, r.clone()));
            }
        }
        for revs in out.values_mut() {
            revs.sort_by(|a, b| b.0.cmp(&a.0)); // newest first
        }
        out
    }

    /// Latest revision of every release (namespace filter applied), sorted by namespace and name.
    fn helm_releases(&self) -> Vec<HelmRel> {
        let mut out: Vec<HelmRel> = self
            .helm_revisions()
            .into_iter()
            .filter(|((_, ns, _), _)| self.ns_filter.is_empty() || self.ns_filter.contains(ns))
            .filter_map(|((ctx, ns, name), revs)| {
                let (revision, status, secret) = revs.into_iter().next()?;
                Some(HelmRel {
                    ctx,
                    ns,
                    name,
                    revision,
                    status,
                    secret,
                })
            })
            .collect();
        out.sort_by(|a, b| (&a.ns, &a.name, &a.ctx.name).cmp(&(&b.ns, &b.name, &b.ctx.name)));
        out
    }

    /// Decoded release JSON for a storage Secret (memoized until the Secret changes).
    fn helm_data(&self, secret: &Arc<Row>) -> Option<Arc<Value>> {
        let key = (secret.ctx.clone(), secret.ns.clone(), secret.name.clone());
        if let Some((row, v)) = self.helm_memo.borrow().get(&key)
            && Arc::ptr_eq(row, secret)
        {
            return Some(v.clone());
        }
        let v = Arc::new(decode_helm(secret, false)?);
        self.helm_memo.borrow_mut().insert(key, (secret.clone(), v.clone()));
        Some(v)
    }

    fn helm_list(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(String::from).collect();
        let many = self.active.len() > 1;
        let releases: Vec<(HelmRel, Option<Arc<Value>>)> = self
            .helm_releases()
            .into_iter()
            .map(|r| {
                let data = self.helm_data(&r.secret);
                (r, data)
            })
            .filter(|(r, d)| {
                let chart = d.as_ref().map(|d| k8s::s(d, "/chart/metadata/name")).unwrap_or_default();
                let line = format!("{} {} {} {} {}", r.name, r.ns, r.status, chart, r.ctx.name).to_lowercase();
                terms.iter().all(|t| line.contains(t))
            })
            .collect();
        let mut cols: Vec<(&str, f32)> = vec![];
        if many {
            cols.push(("Context", 150.));
        }
        cols.extend([
            ("Namespace", 140.),
            ("Name", 0.),
            ("Chart", 0.),
            ("App version", 100.),
            ("Revision", 70.),
            ("Status", 110.),
            ("Updated", 70.),
        ]);
        // Name and Chart share what's left; text is measured so long names end in "…".
        let fixed: f32 = cols.iter().map(|(_, w)| w + COL_GAP).sum::<f32>() + 2. * COL_GAP + 24.;
        let flex_w = ((self.detail_w - fixed) / 2.).max(140.);
        let col = |w: f32| div().mr(px(COL_GAP)).truncate().flex_none().w(px(if w == 0. { flex_w } else { w }));
        let fit = |t: String, w: f32| fit_text(window, cx, t, w);
        let header = div()
            .flex()
            .items_center()
            .h(px(32.))
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(FAINT))
            .children(cols.iter().map(|(h, w)| col(*w).child(h.to_string())));
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .child(header)
            .when(releases.is_empty(), |d| {
                d.child(div().pt_16().flex().justify_center().text_color(rgb(FAINT)).child(if self.loading {
                    "Loading releases…"
                } else {
                    "No Helm releases in the selected clusters."
                }))
            })
            .child(
                div()
                    .id("helm-rows")
                    .flex_1()
                    .overflow_y_scroll()
                    .children(releases.into_iter().enumerate().map(|(i, (r, d))| {
                        let get = |p: &str| d.as_ref().map(|d| k8s::s(d, p)).unwrap_or_default();
                        let chart = format!("{}-{}", get("/chart/metadata/name"), get("/chart/metadata/version"));
                        let updated = k8s::ts(d.as_deref().unwrap_or(&Value::Null), "/info/last_deployed").map_or("–".into(), k8s::age);
                        let color = helm_status_color(&r.status);
                        let sel = (r.ctx.clone(), r.ns.clone(), r.name.clone());
                        div()
                            .id(("helm", i))
                            .flex()
                            .items_center()
                            .h(px(32.))
                            .px_3()
                            .border_b_1()
                            .border_color(rgb(ROWLINE))
                            .cursor_pointer()
                            .hover(|s| s.bg(rgb(HOVER)))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                (this.helm_sel, this.helm_tab) = (Some(sel.clone()), HelmTab::Overview);
                                cx.notify();
                            }))
                            .when(many, |row| {
                                row.child(
                                    col(150.)
                                        .flex()
                                        .items_center()
                                        .gap_1p5()
                                        .text_color(rgb(DIM))
                                        .child(div().flex_none().size(px(8.)).rounded_sm().bg(rgb(self.ctx_color(&r.ctx))))
                                        .child(div().truncate().child(r.ctx.name.clone())),
                                )
                            })
                            .child(col(150.).text_color(rgb(DIM)).child(r.ns.clone()))
                            .child(col(0.).text_color(rgb(TEXT)).child(fit(r.name.clone(), flex_w)))
                            .child(col(0.).text_color(rgb(DIM)).child(fit(chart, flex_w)))
                            .child(col(120.).text_color(rgb(DIM)).child(get("/chart/metadata/appVersion")))
                            .child(col(80.).text_color(rgb(DIM)).child(r.revision.to_string()))
                            .child(
                                col(120.)
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .text_color(rgb(color))
                                    .child(div().flex_none().size(px(6.)).rounded_full().bg(rgb(color)))
                                    .child(r.status.clone()),
                            )
                            .child(col(90.).text_color(rgb(DIM)).child(updated))
                    })),
            )
    }

    /// Text shown in the Values/Defaults/Manifest tabs.
    fn helm_tab_text(&self, data: &Value) -> String {
        let yaml = |v: &Value| match v {
            Value::Object(m) if m.is_empty() => "# (none)\n".to_string(),
            Value::Null => "# (none)\n".to_string(),
            v => serde_yaml::to_string(v).unwrap_or_default(),
        };
        match self.helm_tab {
            HelmTab::Values => yaml(&data["config"]),
            HelmTab::Defaults => yaml(&data["chart"]["values"]),
            HelmTab::Manifest => String::new(), // decoded on demand in sync_helm_text
            _ => String::new(),
        }
    }

    /// Keeps the read-only editor of the open Values/Defaults/Manifest tab (needs the window).
    fn sync_helm_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let wants_text = matches!(self.helm_tab, HelmTab::Values | HelmTab::Defaults | HelmTab::Manifest);
        let rel = self
            .helm_sel
            .clone()
            .filter(|_| self.helm_mode && wants_text)
            .and_then(|(c, ns, n)| self.helm_releases().into_iter().find(|r| r.ctx == c && r.ns == ns && r.name == n));
        let Some(rel) = rel else {
            self.helm_text = None;
            return;
        };
        let Some(data) = self.helm_data(&rel.secret) else { return };
        let key = format!("{}|{}|{}|{}", rel.ctx.name, rel.secret.name, rel.ns, self.helm_tab as u8);
        if self.helm_text.as_ref().is_some_and(|(k, _)| *k == key) {
            return;
        }
        let text = match self.helm_tab {
            HelmTab::Manifest => decode_helm(&rel.secret, true).map(|v| k8s::s(&v, "/manifest")).unwrap_or_default(),
            _ => self.helm_tab_text(&data),
        };
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("yaml")
                .line_number(true)
                .searchable(true)
                .default_value(text)
        });
        self.helm_text = Some((key, input));
    }

    fn helm_detail(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let Some((ctx, ns, name)) = self.helm_sel.clone() else {
            return div().into_any_element();
        };
        let revisions = self
            .helm_revisions()
            .remove(&(ctx.clone(), ns.clone(), name.clone()))
            .unwrap_or_default();
        let Some((revision, status, secret)) = revisions.first().cloned() else {
            return div()
                .p_4()
                .text_color(rgb(FAINT))
                .child("This release no longer exists.")
                .into_any_element();
        };
        let data = self.helm_data(&secret).unwrap_or_default();
        let get = |p: &str| k8s::s(&data, p);
        let tab = |id: &'static str, icon_name: &str, label: &'static str, t: HelmTab| {
            let on = self.helm_tab == t;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1p5()
                .px_2p5()
                .py_0p5()
                .rounded_sm()
                .cursor_pointer()
                .text_color(rgb(if on { TEXT } else { DIM }))
                .when(on, |d| d.bg(rgb(SELECT)))
                .when(!on, |d| d.hover(|s| s.text_color(rgb(TEXT))))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.helm_tab = t;
                    cx.notify();
                }))
                .child(icon(icon_name, if on { ACCENT } else { FAINT }).size(px(13.)))
                .child(label)
        };
        // Flux HelmRelease that owns this release, if any (its history names the Helm release).
        let flux = k8s::find_kind(|k| k.is_flux() && k.kind == "HelmRelease").and_then(|ix| {
            let cache = CACHE.lock().unwrap();
            let r = cache
                .get(&(ctx.clone(), ix))?
                .rows
                .values()
                .find(|r| k8s::s(&r.obj, "/status/history/0/name") == name && k8s::s(&r.obj, "/status/history/0/namespace") == ns)?;
            Some(Link::Object(ix, (ctx.clone(), r.ns.clone(), r.name.clone())))
        });
        let header = div()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
            .border_b_1()
            .border_color(rgb(BORDER))
            .child(
                div()
                    .flex()
                    .items_start()
                    .gap_2()
                    .child(
                        icon_button("helm-back", "arrow-left", DIM, "Back (esc)").on_click(cx.listener(|this, _, _, cx| {
                            this.helm_sel = None;
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).truncate().child(name.clone()))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(FAINT))
                                    .child(format!("Helm release · {} · {ns}", ctx.name)),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .p_0p5()
                    .rounded_md()
                    .bg(rgb(SURFACE))
                    .text_xs()
                    .child(tab("helm-overview", "layout-dashboard", "Overview", HelmTab::Overview))
                    .child(tab("helm-values", "file-code", "Values", HelmTab::Values))
                    .child(tab("helm-defaults", "file-text", "Chart defaults", HelmTab::Defaults))
                    .child(tab("helm-manifest", "file-stack", "Manifest", HelmTab::Manifest))
                    .child(tab("helm-notes", "scroll-text", "Notes", HelmTab::Notes)),
            );
        let body = match self.helm_tab {
            HelmTab::Overview => {
                let color = helm_status_color(&status);
                let mut facts: Vec<Fact> = vec![
                    (
                        "Chart",
                        format!("{} {}", get("/chart/metadata/name"), get("/chart/metadata/version")),
                        None,
                    ),
                    ("App version", get("/chart/metadata/appVersion"), None),
                    ("Status", status.clone(), None),
                    ("Revision", revision.to_string(), None),
                    (
                        "Updated",
                        k8s::ts(&data, "/info/last_deployed")
                            .map(|t| format!("{} ago", k8s::age(t)))
                            .unwrap_or_default(),
                        None,
                    ),
                    ("Description", get("/info/description"), None),
                ];
                if let Some(link) = flux {
                    facts.push(("Managed by", "Flux HelmRelease".into(), Some(link)));
                }
                facts.push((
                    "Storage",
                    secret.name.clone(),
                    self.link(&ctx, kind_ix("Secrets"), &ns, &secret.name),
                ));
                facts.retain(|f| !f.1.trim().is_empty());
                let details = card("Details").children(facts.into_iter().enumerate().map(|(i, (label, value, link))| {
                    let v = div().id(("helm-fact", i)).flex_1().min_w_0().child(value);
                    let v = match link {
                        Some(Link::Object(kind, key)) => v
                            .text_color(rgb(ACCENT))
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_click(cx.listener(move |this, _, _, cx| this.goto(kind, key.clone(), cx))),
                        _ if label == "Status" => v.text_color(rgb(color)),
                        _ => v.text_color(rgb(TEXT)),
                    };
                    div()
                        .flex()
                        .gap_3()
                        .child(div().flex_none().w(px(110.)).text_color(rgb(FAINT)).child(label))
                        .child(v)
                }));
                let history = card(format!("History ({})", revisions.len())).children(revisions.iter().map(|(rev, st, sec)| {
                    let d = self.helm_data(sec).unwrap_or_default();
                    let when = k8s::ts(&d, "/info/last_deployed")
                        .map(|t| format!("{} ago", k8s::age(t)))
                        .unwrap_or_default();
                    let color = helm_status_color(st);
                    div()
                        .flex()
                        .gap_3()
                        .child(div().flex_none().w(px(36.)).text_color(rgb(DIM)).child(format!("#{rev}")))
                        .child(
                            div()
                                .flex_none()
                                .w(px(110.))
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .text_color(rgb(color))
                                .child(div().flex_none().size(px(6.)).rounded_full().bg(rgb(color)))
                                .child(st.clone()),
                        )
                        .child(div().flex_none().w(px(260.)).text_color(rgb(DIM)).child(fit_text(
                            window,
                            cx,
                            format!("{} {}", k8s::s(&d, "/chart/metadata/name"), k8s::s(&d, "/chart/metadata/version")),
                            250.,
                        )))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_color(rgb(FAINT))
                                .child(k8s::s(&d, "/info/description")),
                        )
                        .child(div().flex_none().text_xs().text_color(rgb(FAINT)).child(when))
                }));
                div()
                    .id("helm-overview-body")
                    .flex_1()
                    .min_w_0()
                    .overflow_y_scroll()
                    .px_4()
                    .py_4()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(details)
                    .child(history)
                    .into_any_element()
            }
            HelmTab::Notes => {
                let notes = get("/info/notes");
                div()
                    .id("helm-notes-body")
                    .flex_1()
                    .overflow_y_scroll()
                    .bg(rgb(CODE))
                    .px_4()
                    .py_3()
                    .font_family("Menlo")
                    .text_xs()
                    .text_color(rgb(if notes.is_empty() { FAINT } else { DIM }))
                    .child(if notes.is_empty() {
                        "This chart has no notes.".to_string()
                    } else {
                        notes
                    })
                    .into_any_element()
            }
            _ => match &self.helm_text {
                Some((_, input)) => div()
                    .flex_1()
                    .min_h_0()
                    .bg(rgb(CODE))
                    .font_family("Menlo")
                    .text_xs()
                    .child(Input::new(input).h_full().appearance(false).disabled(true))
                    .into_any_element(),
                None => div().p_4().text_color(rgb(FAINT)).child("Loading…").into_any_element(),
            },
        };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .child(header)
            .child(body)
            .into_any_element()
    }

    /// Namespace names across the selected clusters (from the Namespaces watch).
    fn all_namespaces(&self) -> Vec<String> {
        let cache = CACHE.lock().unwrap();
        let mut names: Vec<String> = self
            .active
            .iter()
            .filter_map(|c| cache.get(&(c.clone(), kind_ix("Namespaces"))))
            .flat_map(|s| s.rows.values().map(|r| r.name.clone()))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    fn set_ns_filter(&mut self, filter: std::collections::BTreeSet<String>, cx: &mut Context<Self>) {
        self.ns_filter = filter;
        if let Err(e) = save_json("namespaces.json", &self.ns_filter) {
            self.status = format!("Could not save namespace filter: {e}");
        }
        self.sync_rows();
        cx.notify();
    }

    fn sync_ns_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match (self.ns_open, &self.ns_search) {
            (true, None) => {
                let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search namespaces…"));
                input.update(cx, |s, cx| s.focus(window, cx));
                cx.subscribe(&input, |_, _, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::Change) {
                        cx.notify()
                    }
                })
                .detach();
                self.ns_search = Some(input);
            }
            (false, Some(_)) => self.ns_search = None,
            _ => {}
        }
    }

    /// Namespace picker: search, "All namespaces", and a checkbox per namespace (multi-select).
    fn ns_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let query = self
            .ns_search
            .as_ref()
            .map(|i| i.read(cx).value().to_lowercase())
            .unwrap_or_default();
        let names: Vec<String> = self.all_namespaces().into_iter().filter(|n| n.contains(&query)).collect();
        let checkbox = |on: bool| {
            let b = div().flex_none().size(px(15.)).rounded_sm().flex().items_center().justify_center();
            if on {
                b.bg(rgb(ACCENT)).child(icon("check", BG).size(px(10.)))
            } else {
                b.border_1().border_color(rgb(FAINT))
            }
        };
        let row = |id: ElementId| {
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_2p5()
                .px_2()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(HOVER)))
        };
        div()
            .id("ns-picker")
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    this.ns_open = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .absolute()
            .top(px(TOP - 4.))
            .right(px(12.))
            .w(px(300.))
            .max_h(px(480.))
            .flex()
            .flex_col()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(0x34374a))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .p_2()
                    .when_some(self.ns_search.clone(), |d, input| d.child(Input::new(&input).small())),
            )
            .child(
                div()
                    .id("ns-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_1p5()
                    .pb_1p5()
                    .flex()
                    .flex_col()
                    .gap_px()
                    .when(query.is_empty(), |d| {
                        d.child(
                            row("ns-all".into())
                                .on_click(cx.listener(|this, _, _, cx| this.set_ns_filter(Default::default(), cx)))
                                .child(checkbox(self.ns_filter.is_empty()))
                                .child(div().text_color(rgb(TEXT)).child("All namespaces")),
                        )
                    })
                    .children(names.into_iter().enumerate().map(|(i, name)| {
                        let on = self.ns_filter.contains(&name);
                        let label = name.clone();
                        row(("ns", i).into())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let mut filter = this.ns_filter.clone();
                                if !filter.remove(&name) {
                                    filter.insert(name.clone());
                                }
                                this.set_ns_filter(filter, cx);
                            }))
                            .child(checkbox(on))
                            .child(div().truncate().text_color(rgb(if on { TEXT } else { DIM })).child(label))
                    })),
            )
    }

    /// Keeps one read-only editor per shown data value of the open ConfigMap/Secret (InputState needs the window).
    fn sync_value_views(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let kind = kind_at(self.kind).name;
        let row = self
            .selected_row()
            .cloned()
            .filter(|_| self.tab == Tab::Overview && matches!(kind, "ConfigMaps" | "Secrets"));
        let Some(row) = row else {
            self.value_views.clear();
            return;
        };
        let secret = kind == "Secrets";
        let shown: HashMap<String, String> = data_entries(&row, secret)
            .into_iter()
            .filter(|(k, _)| !secret || self.revealed.contains(k))
            .collect();
        self.value_views.retain(|k, _| shown.contains_key(k));
        for (k, v) in shown {
            match self.value_views.get(&k) {
                Some(view) if view.read(cx).value() != v.as_str() => view.update(cx, |s, cx| s.set_value(v, window, cx)),
                Some(_) => {}
                None => {
                    let view = cx.new(|cx| InputState::new(window, cx).auto_grow(1, 14).soft_wrap(false).default_value(v));
                    self.value_views.insert(k, view);
                }
            }
        }
    }

    fn current_editor(&mut self) -> Option<&mut YamlEditor> {
        let key = self.selected.clone()?;
        self.editors.get_mut(&(self.kind, key))
    }

    /// Discard edits (or take the cluster's newer version) and show the latest object.
    fn reload_yaml(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.selected_row().cloned() else { return };
        let Some(ed) = self.current_editor() else { return };
        let text = yaml_text(&row);
        ed.state.update(cx, |s, cx| s.set_value(text.clone(), window, cx));
        (ed.base, ed.original, ed.stale, ed.error) = (row, text, false, None);
        cx.notify();
    }

    fn save_yaml(&mut self, cx: &mut Context<Self>) {
        let (kind, Some((ctx, ns, name))) = (kind_at(self.kind), self.selected.clone()) else {
            return;
        };
        let Some(ed) = self.current_editor() else { return };
        if ed.saving {
            return;
        }
        let text = ed.state.read(cx).value().to_string();
        (ed.saving, ed.error) = (true, None);
        let id = (self.kind, (ctx.clone(), ns.clone(), name.clone()));
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = run(k8s::replace(ctx, kind, ns, name.clone(), text.clone(), false)).await;
            this.update(cx, |this, cx| {
                let Some(ed) = this.editors.get_mut(&id) else { return };
                ed.saving = false;
                match res {
                    Ok(()) => {
                        (ed.original, ed.stale, ed.reload_next) = (text, false, true);
                        this.status = format!("Saved {} {name} ✓", kind.kind.to_lowercase());
                    }
                    Err(e) => ed.error = Some(format!("{e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Containers to pick logs from (main containers first, then init containers).
    fn log_containers(&self, row: &Row) -> Vec<String> {
        let spec = match kind_at(self.kind).name {
            "Pods" => &row.obj["spec"],
            "Jobs" | "Deployments" | "StatefulSets" | "DaemonSets" => &row.obj["spec"]["template"]["spec"],
            _ => return vec![],
        };
        let names = |p: &str| {
            k8s::arr(spec, p)
                .iter()
                .filter_map(|c| c["name"].as_str().map(String::from))
                .collect::<Vec<_>>()
        };
        names("/containers").into_iter().chain(names("/initContainers")).collect()
    }

    /// The pods whose logs make up this object's Logs tab.
    fn log_pods(&self, row: &Row) -> Vec<String> {
        if kind_at(self.kind).name == "Pods" {
            return vec![row.name.clone()];
        }
        let mut pods: Vec<String> = self.related_pods(row).unwrap_or_default().iter().map(|p| p.name.clone()).collect();
        pods.sort();
        pods.truncate(20); // ponytail: cap concurrent streams; a page of 20 pods' logs is plenty to read
        pods
    }

    /// Starts/stops the log stream to match the open tab and its options (runs in render).
    fn sync_logs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let row = self
            .selected_row()
            .cloned()
            .filter(|_| self.tab == Tab::Logs && has_logs(kind_at(self.kind).name));
        let Some(row) = row else {
            self.logs = None;
            self.log_search = None;
            return;
        };
        let containers = self.log_containers(&row);
        let container = self
            .container
            .clone()
            .filter(|c| containers.contains(c))
            .or_else(|| containers.first().cloned());
        let previous = self.log_previous && kind_at(self.kind).name == "Pods";
        let key: LogKey = (
            self.kind,
            (row.ctx.clone(), row.ns.clone(), row.name.clone()),
            container.clone(),
            previous,
            self.log_timestamps,
        );
        if self.logs.as_ref().map(|s| &s.key) != Some(&key) {
            let stream = k8s::stream_logs(
                row.ctx.clone(),
                row.ns.clone(),
                self.log_pods(&row),
                container,
                previous,
                self.log_timestamps,
            );
            self.logs = Some(LogSession { key, stream, seen: 0 });
            self.log_follow = true;
        }
        if self.log_search.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Filter lines…"));
            cx.subscribe(&input, |_, _, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify()
                }
            })
            .detach();
            self.log_search = Some(input);
        }
        // Wrapping splits lines by character count, which works because the log font is monospace.
        // ponytail: wide chars (CJK, emoji) count as one column, so such lines wrap a bit late.
        let cols = if self.log_wrap {
            let width = self.log_scroll.0.borrow().base_handle.bounds().size.width;
            if width <= px(0.) {
                cx.on_next_frame(window, |_, _, cx| cx.notify()); // not laid out yet
            }
            let font_id = window.text_system().resolve_font(&gpui::font("Menlo"));
            let char_w = window
                .text_system()
                .advance(font_id, window.rem_size() * 0.75, 'm')
                .map_or(px(7.2), |s| s.width);
            let pod_tag = if self.logs.as_ref().is_some_and(|s| s.stream.pods.len() > 1) {
                60.
            } else {
                0.
            };
            let text_w = width - px(24. + 11. + pod_tag); // padding, level bar + gap, pod tag + gap
            ((text_w / char_w).floor() as usize).max(20)
        } else {
            usize::MAX
        };
        // Filter, then keep the newest line in view while following.
        let query = self
            .log_search
            .as_ref()
            .map(|i| i.read(cx).value().to_lowercase())
            .unwrap_or_default();
        let session = self.logs.as_ref().expect("just ensured");
        let lines = session.stream.lines.lock().unwrap();
        self.log_view.clear();
        self.log_widest = 0;
        let mut widest = 0;
        for (i, line) in lines.iter().enumerate() {
            let t = &line.text;
            if !query.is_empty() && !t.to_lowercase().contains(&query) {
                continue;
            }
            if t.len() > widest {
                (widest, self.log_widest) = (t.len(), self.log_view.len());
            }
            if t.len() <= cols {
                self.log_view.push((i, 0..t.len()));
                continue;
            }
            let (mut start, mut n) = (0, 0);
            for (b, _) in t.char_indices() {
                if n == cols {
                    self.log_view.push((i, start..b));
                    (start, n) = (b, 0);
                }
                n += 1;
            }
            self.log_view.push((i, start..t.len()));
        }
        drop(lines);
        if self.log_follow && !self.log_view.is_empty() {
            self.log_scroll.scroll_to_item(self.log_view.len() - 1, ScrollStrategy::Bottom);
        }
    }

    /// The filtered log lines as text (for Copy).
    fn log_text(&self) -> String {
        let Some(session) = &self.logs else { return String::new() };
        let lines = session.stream.lines.lock().unwrap();
        self.log_view
            .iter()
            .filter(|(_, r)| r.start == 0)
            .map(|(i, _)| lines[*i].text.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn logs_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row = self.selected_row().cloned();
        let containers = row.as_ref().map(|r| self.log_containers(r)).unwrap_or_default();
        let current = self.logs.as_ref().and_then(|s| s.key.2.clone());
        let pods = self.logs.as_ref().map(|s| s.stream.pods.clone()).unwrap_or_default();
        let total = self.logs.as_ref().map_or(0, |s| s.stream.lines.lock().unwrap().len());
        let is_pod = kind_at(self.kind).name == "Pods";
        let toggle = |id: &'static str, label: &'static str, on: bool| {
            button(id, if on { "check" } else { "" }, label, if on { ACCENT } else { DIM }).when(on, |b| b.border_color(rgb(ACCENT)))
        };
        let rows = self.log_view.len();
        let shown = self.log_view.iter().filter(|(_, r)| r.start == 0).count();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(CODE))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .text_xs()
                    .child(
                        div()
                            .w(px(240.))
                            .when_some(self.log_search.clone(), |d, input| d.child(Input::new(&input).small())),
                    )
                    .when(containers.len() > 1, |d| {
                        d.children(containers.into_iter().enumerate().map(|(i, c)| {
                            let on = current.as_ref() == Some(&c);
                            button(("container", i), "", c.clone(), DIM)
                                .when(on, |b| b.bg(rgb(SELECT)).text_color(rgb(TEXT)))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.container = Some(c.clone());
                                    cx.notify();
                                }))
                        }))
                    })
                    .child(
                        toggle("log-follow", "Follow", self.log_follow).on_click(cx.listener(|this, _, _, cx| {
                            this.log_follow = !this.log_follow;
                            cx.notify();
                        })),
                    )
                    .child(toggle("log-wrap", "Wrap", self.log_wrap).on_click(cx.listener(|this, _, _, cx| {
                        this.log_wrap = !this.log_wrap;
                        this.save_settings();
                        cx.notify();
                    })))
                    .child(
                        toggle("log-ts", "Timestamps", self.log_timestamps).on_click(cx.listener(|this, _, _, cx| {
                            this.log_timestamps = !this.log_timestamps;
                            cx.notify();
                        })),
                    )
                    .when(is_pod, |d| {
                        d.child(
                            toggle("log-prev", "Previous", self.log_previous).on_click(cx.listener(|this, _, _, cx| {
                                this.log_previous = !this.log_previous;
                                cx.notify();
                            })),
                        )
                    })
                    .child(div().flex_1())
                    .child(div().text_color(rgb(FAINT)).child(match (shown == total, pods.len()) {
                        (true, 1) => format!("{total} lines"),
                        (true, n) => format!("{total} lines · {n} pods"),
                        (false, _) => format!("{shown} of {total} lines"),
                    })),
            )
            .child(if pods.is_empty() {
                div()
                    .p_4()
                    .text_color(rgb(FAINT))
                    .child("No pods to show logs for.")
                    .into_any_element()
            } else {
                uniform_list(
                    "log-lines",
                    rows,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        let Some(session) = &this.logs else { return vec![] };
                        let lines = session.stream.lines.lock().unwrap();
                        let many = session.stream.pods.len() > 1;
                        range
                            .filter_map(|vi| {
                                let (li, part) = this.log_view.get(vi)?.clone();
                                let line = lines.get(li)?;
                                let text = line.text.clone();
                                let first = part.start == 0;
                                // Pod tag: the hash suffix tells replicas apart.
                                let pod = session
                                    .stream
                                    .pods
                                    .get(line.pod)
                                    .map(|p| p.rsplit('-').next().unwrap_or(p).to_string());
                                let color = CTX_COLORS[line.pod % CTX_COLORS.len()];
                                let warn = text.starts_with('⚠');
                                Some(
                                    div()
                                        .id(("log", vi))
                                        .flex()
                                        .gap_2()
                                        .h(px(18.))
                                        .px_3()
                                        .whitespace_nowrap()
                                        .cursor_pointer()
                                        .hover(|s| s.bg(rgb(HOVER)))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                                            this.status = "Log line copied".into();
                                            cx.notify();
                                        }))
                                        .child(
                                            div()
                                                .flex_none()
                                                .w(px(3.))
                                                .h_full()
                                                .when_some(log_level(&line.text), |d, c| d.bg(rgb(c))),
                                        )
                                        .when(many, |d| {
                                            d.child(
                                                div()
                                                    .flex_none()
                                                    .w(px(52.))
                                                    .text_color(rgb(color))
                                                    .when(first, |d| d.child(pod.unwrap_or_default())),
                                            )
                                        })
                                        .child(
                                            div()
                                                .flex_none()
                                                .text_color(rgb(if warn { YELLOW } else { DIM }))
                                                .child(line.text[part].to_string()),
                                        ),
                                )
                            })
                            .collect()
                    }),
                )
                .track_scroll(self.log_scroll.clone())
                .when(!self.log_wrap, |l| {
                    l.with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
                        .with_width_from_item(Some(self.log_widest))
                })
                .flex_1()
                .font_family("Menlo")
                .text_xs()
                // Scrolling up pauses Follow; Follow resumes from the toolbar.
                .on_scroll_wheel(cx.listener(|this, ev: &gpui::ScrollWheelEvent, _, cx| {
                    if ev.delta.pixel_delta(px(18.)).y > px(0.) && this.log_follow {
                        this.log_follow = false;
                        cx.notify();
                    }
                }))
                .into_any_element()
            })
    }

    /// Runs an action on an object; destructive ones ask first.
    /// Runs an action on an object right away (no confirmation, by request).
    fn act(&mut self, action: Action, kind_ix_: usize, key: Key, cx: &mut Context<Self>) {
        // `flux reconcile --with-source`: refresh the source(s) first, then the object.
        let sources: Vec<(usize, String, String)> = match action {
            Action::ReconcileWithSource => self.cached_row(kind_ix_, &key).map(|r| flux_sources(&r)).unwrap_or_default(),
            _ => vec![],
        };
        let (ctx, ns, name) = key;
        let kind = kind_at(kind_ix_);
        let what = format!("{} {name}", kind.kind.to_lowercase());
        self.status = format!("{}: {what}…", action.label());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let res = match action {
                Action::Delete => run(k8s::delete(ctx, kind, ns, name, false))
                    .await
                    .map(|_| format!("Deleted {what}")),
                Action::Redeploy => run(k8s::restart(ctx, kind, ns, name, false))
                    .await
                    .map(|_| format!("Redeploying {what}")),
                Action::Trigger => run(k8s::trigger(ctx, ns, name, false))
                    .await
                    .map(|job| format!("Triggered {what} → job {job}")),
                Action::ReconcileWithSource => {
                    let mut done = vec![];
                    for (six, sns, sname) in sources {
                        if run(k8s::patch(
                            ctx.clone(),
                            kind_at(six),
                            sns,
                            sname.clone(),
                            k8s::reconcile_patch(),
                            false,
                        ))
                        .await
                        .is_ok()
                        {
                            done.push(format!("{}/{sname}", kind_at(six).kind.to_lowercase()));
                        }
                    }
                    run(k8s::patch(ctx, kind, ns, name, k8s::reconcile_patch(), false)).await.map(|_| {
                        format!(
                            "Reconcile requested for {what} (sources: {})",
                            if done.is_empty() { "none found".into() } else { done.join(", ") }
                        )
                    })
                }
                Action::Reconcile => run(k8s::patch(ctx, kind, ns, name, k8s::reconcile_patch(), false))
                    .await
                    .map(|_| format!("Reconcile requested for {what}")),
                Action::Suspend => run(k8s::patch(
                    ctx,
                    kind,
                    ns,
                    name,
                    serde_json::json!({"spec": {"suspend": true}}),
                    false,
                ))
                .await
                .map(|_| format!("Suspended {what}")),
                Action::Resume => {
                    // Like `flux resume`: unsuspend and ask for a reconcile right away.
                    let mut patch = k8s::reconcile_patch();
                    patch["spec"] = serde_json::json!({"suspend": false});
                    run(k8s::patch(ctx, kind, ns, name, patch, false))
                        .await
                        .map(|_| format!("Resumed {what}"))
                }
            };
            this.update(cx, |this, cx| {
                this.status = match res {
                    Ok(msg) => format!("{msg} ✓"),
                    Err(e) => format!("{} {what} failed: {e:#}", action.label()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Current page's sort as (column index, ascending); None if unsorted or that column isn't shown.
    fn sort(&self) -> Option<(usize, bool)> {
        let kind = kind_at(self.kind).name;
        // Your saved choice wins; otherwise a sensible default (newest first where recency matters).
        let (header, asc) = self.sorts.get(kind).cloned().or_else(|| match kind {
            "Events" => Some(("Last seen".into(), true)),
            "Jobs" => Some(("Age".into(), true)),
            _ => None,
        })?;
        Some((self.columns().iter().position(|c| *c == header)?, asc))
    }

    fn columns(&self) -> Vec<&'static str> {
        let kind = kind_at(self.kind);
        let mut cols = vec![];
        if self.active.len() > 1 {
            cols.push("Context");
        }
        if kind.namespaced {
            cols.push("Namespace");
        }
        // An event's name is a generated ID; its Object column says what it's about.
        if kind.name != "Events" {
            cols.push("Name");
        }
        cols.extend(kind.cols);
        if matches!(
            kind.name,
            "Pods" | "Deployments" | "StatefulSets" | "DaemonSets" | "CronJobs" | "Jobs"
        ) {
            cols.push("Image");
        }
        cols.extend(match kind.name {
            "Pods" => &["CPU", "Memory"][..],
            "Nodes" => &["CPU", "Memory", "Pods"][..],
            _ => &[],
        });
        cols.push(if kind.name == "Events" { "Last seen" } else { "Age" });
        cols
    }

    fn cells(&self, r: &Row) -> Vec<String> {
        let mut c = vec![];
        if self.active.len() > 1 {
            c.push(r.ctx.name.clone());
        }
        if kind_at(self.kind).namespaced {
            c.push(r.ns.clone());
        }
        if kind_at(self.kind).name != "Events" {
            c.push(r.name.clone());
        }
        c.extend(r.cells.iter().cloned());
        let spec = match kind_at(self.kind).name {
            "Pods" => Some(&r.obj["spec"]),
            "Deployments" | "StatefulSets" | "DaemonSets" | "Jobs" => Some(&r.obj["spec"]["template"]["spec"]),
            "CronJobs" => Some(&r.obj["spec"]["jobTemplate"]["spec"]["template"]["spec"]),
            _ => None,
        };
        if let Some(spec) = spec {
            c.push(
                k8s::arr(spec, "/containers")
                    .iter()
                    .map(|c| short_image(&k8s::s(c, "/image")))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
        }
        match kind_at(self.kind).name {
            "Pods" => {
                let u = self.pod_usage(r);
                c.push(u.map_or("–".into(), |u| fmt_cpu(u.cpu)));
                c.push(u.map_or("–".into(), |u| fmt_mem(u.mem)));
            }
            "Nodes" => {
                let (cap, cap_pods) = capacity(&r.obj);
                let a = self.alloc.get(&(r.ctx.clone(), r.name.clone())).copied().unwrap_or_default();
                let u = self.node_usage(r);
                c.push(u.map_or("–".into(), |u| pct(u.cpu, cap.cpu)));
                c.push(u.map_or("–".into(), |u| pct(u.mem, cap.mem)));
                c.push(format!("{}/{cap_pods}", a.pods));
            }
            _ => {}
        }
        c.push(k8s::age(r.created));
        c
    }

    /// Pod CPU/Memory table cell: mini bar (request/limit underneath, usage on top) + value, details on hover.
    fn pod_usage_cell(&self, r: &Row, h: &'static str, v: String, vi: usize) -> AnyElement {
        let mem = h == "Memory";
        let pick = |u: Usage| if mem { u.mem } else { u.cpu };
        let fmt: fn(f64) -> String = if mem { fmt_mem } else { fmt_cpu };
        let (req, lim) = k8s::pod_resources(&r.obj);
        let (req, lim, used) = (pick(req), pick(lim), self.pod_usage(r).map(pick));
        let scale = if lim > 0. { lim } else { req };
        let opt = |x: f64| if x > 0. { fmt(x) } else { "none".into() };
        let tooltip = format!(
            "{} used  ·  request {}  ·  limit {}",
            used.map_or("–".into(), fmt),
            opt(req),
            opt(lim)
        );
        // Only near a *limit* is risky (throttling / OOM kill); going over a request is normal.
        let frac = used.filter(|_| lim > 0.).map_or(0., |u| (u / lim) as f32);
        let used_color = if lim > 0. { level_color(frac) } else { GREEN };
        let bar = if scale > 0. {
            // The track is the limit (or request); only draw a request layer when it's below a limit.
            let req_layer = if lim > 0. { req } else { 0. };
            let layers = [(req_layer, accent_alpha(0.5)), (used.unwrap_or(0.), rgb(used_color).into())];
            stacked_bar(scale, &layers).w(px(36.)).h(px(5.))
        } else {
            div().flex_none().w(px(36.))
        };
        cell(h, 0.) // CPU/Memory are fixed-width
            .id((h, vi))
            .flex()
            .items_center()
            .gap_2()
            .text_color(rgb(if frac >= 0.9 {
                RED
            } else if frac >= 0.7 {
                YELLOW
            } else {
                DIM
            }))
            .child(bar)
            .child(v)
            .tooltip(tip(tooltip))
            .into_any_element()
    }

    /// Node CPU/Memory table cell: same layered mini bar as pods, scaled to allocatable.
    fn node_usage_cell(&self, r: &Row, h: &'static str, v: String, vi: usize) -> AnyElement {
        let mem = h == "Memory";
        let pick = |u: Usage| if mem { u.mem } else { u.cpu };
        let fmt: fn(f64) -> String = if mem { fmt_mem } else { fmt_cpu };
        let (cap, _) = capacity(&r.obj);
        let a = self.alloc.get(&(r.ctx.clone(), r.name.clone())).copied().unwrap_or_default();
        let (total, req, lim, used) = (pick(cap), pick(a.req), pick(a.lim), self.node_usage(r).map(pick));
        let pct = |x: f64| if total > 0. { x / total * 100. } else { 0. };
        let tooltip = format!(
            "{}  ·  requested {} ({:.0}%)  ·  limits {} ({:.0}%)  ·  of {} allocatable",
            used.map_or("no metrics".into(), |u| format!("used {} ({:.0}%)", fmt(u), pct(u))),
            fmt(req),
            pct(req),
            fmt(lim),
            pct(lim),
            fmt(total)
        );
        let frac = used.map_or(0., |u| (pct(u) / 100.) as f32);
        let layers = [
            (lim, accent_alpha(0.2)),
            (req, accent_alpha(0.5)),
            (used.unwrap_or(0.), rgb(level_color(frac)).into()),
        ];
        cell(h, 0.) // CPU/Memory are fixed-width
            .id((h, vi))
            .flex()
            .items_center()
            .gap_2()
            .text_color(rgb(if frac >= 0.9 {
                RED
            } else if frac >= 0.7 {
                YELLOW
            } else {
                DIM
            }))
            .child(stacked_bar(total, &layers).w(px(36.)).h(px(5.)))
            .child(v)
            .tooltip(tip(tooltip))
            .into_any_element()
    }

    /// Forwardable ports of a Pod, Service or workload (resolved to a ready pod like kubectl does).
    fn forward_targets(&self, r: &Row) -> Vec<FwdTarget> {
        let ready = |p: &Row| {
            k8s::s(&p.obj, "/status/phase") == "Running" && k8s::arr(&p.obj, "/status/containerStatuses").iter().all(|c| c["ready"] == true)
        };
        let pick_pod = || self.related_pods(r).and_then(|pods| pods.into_iter().find(|p| ready(p)));
        // (port, port name, container name) of TCP container ports.
        let container_ports = |spec: &Value| -> Vec<(u16, String, String)> {
            k8s::arr(spec, "/containers")
                .iter()
                .flat_map(|c| k8s::arr(c, "/ports").iter().map(move |p| (c, p)))
                .filter(|(_, p)| k8s::s(p, "/protocol").is_empty() || k8s::s(p, "/protocol") == "TCP")
                .filter_map(|(c, p)| Some((p["containerPort"].as_u64()? as u16, k8s::s(p, "/name"), k8s::s(c, "/name"))))
                .collect()
        };
        match kind_at(self.kind).name {
            "Pods" => container_ports(&r.obj["spec"])
                .into_iter()
                .map(|(port, _, c)| FwdTarget {
                    label: format!("{c}:{port}"),
                    remote: port,
                    want: port,
                    pod: Some(r.name.clone()),
                })
                .collect(),
            "Deployments" | "StatefulSets" | "DaemonSets" => {
                let pod = pick_pod().map(|p| p.name.clone());
                container_ports(&r.obj["spec"]["template"]["spec"])
                    .into_iter()
                    .map(|(port, _, c)| FwdTarget {
                        label: format!("{c}:{port}"),
                        remote: port,
                        want: port,
                        pod: pod.clone(),
                    })
                    .collect()
            }
            "Services" => {
                let pod = pick_pod();
                k8s::arr(&r.obj, "/spec/ports")
                    .iter()
                    .filter(|p| k8s::s(p, "/protocol").is_empty() || k8s::s(p, "/protocol") == "TCP")
                    .filter_map(|p| {
                        let port = p["port"].as_u64()? as u16;
                        // targetPort: a number, a named container port, or (absent) the same as port.
                        let remote = match &p["targetPort"] {
                            Value::Number(n) => n.as_u64()? as u16,
                            Value::String(name) => pod.as_ref().and_then(|pod| {
                                container_ports(&pod.obj["spec"])
                                    .into_iter()
                                    .find(|(_, n, _)| n == name)
                                    .map(|(p, ..)| p)
                            })?,
                            _ => port,
                        };
                        let name = Some(k8s::s(p, "/name")).filter(|n| !n.is_empty()).unwrap_or_else(|| r.name.clone());
                        let label = if remote == port {
                            format!("{name}:{port}")
                        } else {
                            format!("{name}:{port} → {remote}")
                        };
                        Some(FwdTarget {
                            label,
                            remote,
                            want: port,
                            pod: pod.as_ref().map(|p| p.name.clone()),
                        })
                    })
                    .collect()
            }
            _ => vec![],
        }
    }

    fn forward_of(&self, r: &Row, t: &FwdTarget) -> Option<&FwdEntry> {
        let what = format!("{}/{}", kind_at(self.kind).kind.to_lowercase(), r.name);
        self.forwards
            .iter()
            .find(|f| f.ctx == r.ctx && f.ns == r.ns && f.what == what && f.remote == t.remote)
    }

    fn open_fwd_modal(&mut self, row: Arc<Row>, sel: usize, cx: &mut Context<Self>) {
        self.fwd_modal = Some(FwdModal {
            row,
            sel,
            input: None,
            asked: None,
            suggested: None,
            focus: true,
            picking: false,
            busy: false,
            error: None,
        });
        cx.notify();
    }

    /// `local`: a port the user chose (used exactly); None picks a free one near the remote port.
    /// From the dialog, success closes it and failure is shown inside it.
    fn start_forward(&mut self, r: &Row, t: FwdTarget, local: Option<u16>, cx: &mut Context<Self>) {
        let from_modal = self.fwd_modal.is_some();
        let fail = move |this: &mut Self, msg: String| match this.fwd_modal.as_mut().filter(|_| from_modal) {
            Some(m) => (m.error, m.busy) = (Some(msg), false),
            None => this.status = msg,
        };
        let Some(pod) = t.pod.clone() else {
            fail(self, "No ready pod to forward to".into());
            return cx.notify();
        };
        if let Some(f) = self.forward_of(r, &t) {
            let msg = format!("Already forwarded on localhost:{}", f.fwd.local);
            fail(self, msg);
            return cx.notify();
        }
        if let Some(m) = self.fwd_modal.as_mut() {
            (m.busy, m.error) = (true, None);
        }
        let (ctx, ns) = (r.ctx.clone(), r.ns.clone());
        let what = format!("{}/{}", kind_at(self.kind).kind.to_lowercase(), r.name);
        cx.spawn(async move |this, cx| {
            let res = run(k8s::port_forward(
                ctx.clone(),
                ns.clone(),
                pod.clone(),
                t.remote,
                local.unwrap_or(t.want),
                local.is_some(),
            ))
            .await;
            this.update(cx, |this, cx| {
                match res {
                    Ok(fwd) => {
                        this.status = format!("Forwarding localhost:{} → {what}:{}", fwd.local, t.want);
                        if this.open_browser {
                            let scheme = if matches!(t.remote, 443 | 8443) { "https" } else { "http" };
                            cx.open_url(&format!("{scheme}://localhost:{}", fwd.local));
                        }
                        this.forwards.push(FwdEntry {
                            ctx,
                            ns,
                            what,
                            pod,
                            remote: t.remote,
                            fwd,
                        });
                        if from_modal {
                            this.fwd_modal = None;
                        }
                    }
                    Err(e) => fail(this, format!("Port-forward failed: {e:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn submit_fwd_modal(&mut self, cx: &mut Context<Self>) {
        let Some(m) = &self.fwd_modal else { return };
        if m.busy {
            return;
        }
        let row = m.row.clone();
        let Some(t) = self.forward_targets(&row).into_iter().nth(m.sel) else {
            return;
        };
        let typed = m.input.as_ref().map(|i| i.read(cx).value().trim().to_string()).unwrap_or_default();
        let local = match typed.parse::<u16>() {
            _ if typed.is_empty() => None,
            Ok(p) if p > 0 => Some(p),
            _ => {
                if let Some(m) = self.fwd_modal.as_mut() {
                    m.error = Some(format!("\"{typed}\" isn't a valid port (1–65535)"));
                }
                return cx.notify();
            }
        };
        self.start_forward(&row, t, local, cx);
    }

    /// Creates the dialog's port field (needs the window), fills in a free-port suggestion for the
    /// selected container port, and focuses + selects it so typing replaces it.
    fn sync_fwd_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(row) = self.fwd_modal.as_ref().map(|m| m.row.clone()) else {
            return;
        };
        let targets = self.forward_targets(&row);
        let Some(m) = self.fwd_modal.as_mut() else { return };
        let input = match &m.input {
            Some(input) => input.clone(),
            None => {
                let input = cx.new(|cx| InputState::new(window, cx));
                cx.subscribe(&input, |this: &mut Self, _, ev: &InputEvent, cx| {
                    if matches!(ev, InputEvent::PressEnter { .. }) {
                        this.submit_fwd_modal(cx);
                    }
                })
                .detach();
                m.input = Some(input.clone());
                input
            }
        };
        if let Some((_, port)) = m.suggested.filter(|(sel, _)| *sel == m.sel) {
            m.suggested = None;
            input.update(cx, |s, cx| s.set_value(port.to_string(), window, cx));
            if input.read(cx).focus_handle(cx).is_focused(window) {
                window.dispatch_action(Box::new(SelectAll), cx);
            }
        }
        if m.asked != Some(m.sel) {
            m.asked = Some(m.sel);
            if let Some(t) = targets.get(m.sel) {
                let (sel, want) = (m.sel, t.want);
                cx.spawn(async move |this, cx| {
                    if let Ok(port) = run(k8s::suggest_port(want)).await {
                        this.update(cx, |this, cx| {
                            if let Some(m) = this.fwd_modal.as_mut() {
                                m.suggested = Some((sel, port));
                            }
                            cx.notify();
                        })
                        .ok();
                    }
                })
                .detach();
            }
        }
        if m.focus {
            m.focus = false;
            input.update(cx, |s, cx| s.focus(window, cx));
            window.dispatch_action(Box::new(SelectAll), cx);
        }
    }

    /// (pod, first container) to open a shell in: the pod itself, or a ready pod of a workload.
    fn shell_target(&self, r: &Row) -> Option<(String, String)> {
        if !has_logs(kind_at(self.kind).name) {
            return None;
        }
        let running = |p: &Row| k8s::s(&p.obj, "/status/phase") == "Running";
        let (name, obj) = match kind_at(self.kind).name {
            "Pods" if running(r) => (r.name.clone(), r.obj.clone()),
            "Pods" => return None,
            _ => {
                let p = self.related_pods(r)?.into_iter().find(|p| running(p))?;
                (p.name.clone(), p.obj.clone())
            }
        };
        let container = k8s::arr(&obj, "/spec/containers").first()?["name"].as_str()?.to_string();
        Some((name, container))
    }

    /// Overview card: each forwardable port; Forward… opens the dialog with it selected.
    fn ports_card(&self, r: &Arc<Row>, cx: &mut Context<Self>) -> Option<Div> {
        let targets = self.forward_targets(r);
        if targets.is_empty() {
            return None;
        }
        Some(card("Ports").children(targets.into_iter().enumerate().map(|(i, t)| {
            let active = self.forward_of(r, &t).map(|f| f.fwd.local);
            let via = match &t.pod {
                Some(pod) if *pod != r.name => format!("via {pod}"),
                Some(_) => String::new(),
                None => "no ready pod".into(),
            };
            let row = r.clone();
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_none()
                        .w(px(220.))
                        .truncate()
                        .font_family("Menlo")
                        .text_xs()
                        .child(t.label.clone()),
                )
                .child(div().flex_1().min_w_0().truncate().text_xs().text_color(rgb(FAINT)).child(via))
                .map(|d| match active {
                    Some(local) => d
                        .child(
                            div()
                                .id(("fwd-url", i))
                                .text_xs()
                                .text_color(rgb(ACCENT))
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .on_click(move |_, _, cx| cx.open_url(&format!("http://localhost:{local}")))
                                .child(format!("localhost:{local}")),
                        )
                        .child(
                            button(("fwd-stop", i), "square", "Stop", RED).on_click(cx.listener(move |this, _, _, cx| {
                                this.forwards.retain(|f| f.fwd.local != local);
                                cx.notify();
                            })),
                        ),
                    None => d.child(
                        button(
                            ("fwd-start", i),
                            "arrow-left-right",
                            "Forward…",
                            if t.pod.is_some() { DIM } else { FAINT },
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.open_fwd_modal(row.clone(), i, cx))),
                    ),
                })
        })))
    }

    fn open_browser_toggle(&self, id: &'static str, cx: &mut Context<Self>) -> impl IntoElement {
        let on = self.open_browser;
        let checkbox = div().flex_none().size(px(16.)).rounded_sm().flex().items_center().justify_center();
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_2()
            .text_color(rgb(TEXT))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.open_browser = !this.open_browser;
                this.save_settings();
                cx.notify();
            }))
            .child(if on {
                checkbox.bg(rgb(ACCENT)).child(icon("check", BG).size(px(11.)))
            } else {
                checkbox.border_1().border_color(rgb(FAINT))
            })
            .child("Open in browser")
    }

    /// The "Port forward" dialog: context · Kind, ns / name; Local Port → Container Port; open in browser.
    fn fwd_dialog(&self, m: &FwdModal, cx: &mut Context<Self>) -> impl IntoElement {
        let r = &m.row;
        let targets = self.forward_targets(r);
        let current = targets.get(m.sel).cloned();
        let field_label = |t: &'static str| div().text_sm().text_color(rgb(DIM)).child(t);
        let ns = if r.ns.is_empty() { String::new() } else { format!("{} / ", r.ns) };
        div()
            .id("fwd-modal")
            // Esc closes (the dropdown first) even while the port field has focus.
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                if ev.keystroke.key == "escape" {
                    match this.fwd_modal.as_mut() {
                        Some(m) if m.picking => m.picking = false,
                        _ => this.fwd_modal = None,
                    }
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .w(px(520.))
            .flex()
            .flex_col()
            .rounded_xl()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(0x34374a))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .items_start()
                    .px_6()
                    .pt_5()
                    .pb_4()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(DIM))
                                    .child(format!("{} · {}", r.ctx.name, kind_at(self.kind).kind)),
                            )
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .truncate()
                                    .child(format!("{ns}{}", r.name)),
                            ),
                    )
                    .child(
                        div()
                            .id("fwd-modal-close")
                            .mt_1()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.fwd_modal = None;
                                cx.notify();
                            }))
                            .child(icon("x", DIM).size(px(18.))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_5()
                    .px_6()
                    .pt_5()
                    .pb_5()
                    .child(
                        div()
                            .flex()
                            .items_end()
                            .gap_3()
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1p5()
                                    .child(field_label("Local Port"))
                                    .child(div().w(px(120.)).when_some(m.input.clone(), |d, input| d.child(Input::new(&input)))),
                            )
                            .child(div().pb_2().text_color(rgb(DIM)).child("→"))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .gap_1p5()
                                    .child(field_label("Container Port"))
                                    .child(
                                        div()
                                            .relative()
                                            .child(
                                                div()
                                                    .id("fwd-port-picker")
                                                    .flex()
                                                    .items_center()
                                                    .gap_2()
                                                    .h(px(34.))
                                                    .px_3()
                                                    .rounded_md()
                                                    .border_1()
                                                    .border_color(rgb(if m.picking { ACCENT } else { BORDER }))
                                                    .bg(rgb(BG))
                                                    .cursor_pointer()
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        if let Some(m) = this.fwd_modal.as_mut() {
                                                            m.picking = !m.picking;
                                                        }
                                                        cx.notify();
                                                    }))
                                                    .child(
                                                        div()
                                                            .flex_1()
                                                            .min_w_0()
                                                            .truncate()
                                                            .child(current.as_ref().map_or("No ports".to_string(), |t| t.label.clone())),
                                                    )
                                                    .child(icon("chevron-down", DIM).size(px(14.))),
                                            )
                                            .when(m.picking, |d| {
                                                d.child(
                                                    div()
                                                        .absolute()
                                                        .top(px(38.))
                                                        .left_0()
                                                        .w_full()
                                                        .p_1()
                                                        .rounded_md()
                                                        .bg(rgb(SURFACE))
                                                        .border_1()
                                                        .border_color(rgb(0x34374a))
                                                        .shadow_lg()
                                                        .children(targets.iter().enumerate().map(|(i, t)| {
                                                            let on = i == m.sel;
                                                            div()
                                                                .id(("fwd-port-opt", i))
                                                                .px_2()
                                                                .py_1p5()
                                                                .rounded_sm()
                                                                .cursor_pointer()
                                                                .truncate()
                                                                .when(on, |d| d.bg(rgb(SELECT)))
                                                                .hover(|s| s.bg(rgb(HOVER)))
                                                                .on_click(cx.listener(move |this, _, _, cx| {
                                                                    if let Some(m) = this.fwd_modal.as_mut() {
                                                                        (m.sel, m.picking) = (i, false);
                                                                    }
                                                                    cx.notify();
                                                                }))
                                                                .child(t.label.clone())
                                                        })),
                                                )
                                            }),
                                    ),
                            ),
                    )
                    .child(self.open_browser_toggle("open-browser-modal", cx))
                    .when_some(
                        current.as_ref().and_then(|t| t.pod.clone()).filter(|pod| *pod != r.name),
                        |d, pod| d.child(div().text_xs().text_color(rgb(FAINT)).child(format!("Connects to pod {pod}"))),
                    )
                    .when(current.as_ref().is_some_and(|t| t.pod.is_none()), |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(rgb(YELLOW))
                                .child("No ready pod to forward to right now."),
                        )
                    })
                    .when_some(m.error.clone(), |d, e| d.child(div().text_xs().text_color(rgb(RED)).child(e)))
                    .child(
                        div().flex().justify_end().child(
                            div()
                                .id("fwd-start")
                                .px_4()
                                .py_2()
                                .rounded_md()
                                .bg(rgb(if m.busy { 0x3d59a1 } else { 0x4c6fdc }))
                                .text_color(rgb(0xffffff))
                                .font_weight(FontWeight::MEDIUM)
                                .cursor_pointer()
                                .hover(|s| s.bg(rgb(0x5b7ce6)))
                                .on_click(cx.listener(|this, _, _, cx| this.submit_fwd_modal(cx)))
                                .child(if m.busy { "Starting…" } else { "Start Port Forward" }),
                        ),
                    ),
            )
    }

    /// Bottom-of-sidebar list of running forwards: ● :port → name. Port opens the browser; hover to stop.
    fn forwards_strip(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex_none()
            .border_t_1()
            .border_color(rgb(BORDER))
            .px_2()
            .pt_2()
            .pb_3()
            .flex()
            .flex_col()
            .gap_px()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(section("arrow-left-right", "PORT FORWARDS"))
                    .when(self.forwards.len() > 1, |d| {
                        d.child(
                            div()
                                .id("fwd-stop-all")
                                .pb_1()
                                .pr_2()
                                .text_xs()
                                .text_color(rgb(FAINT))
                                .cursor_pointer()
                                .hover(|s| s.text_color(rgb(RED)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.forwards.clear();
                                    cx.notify();
                                }))
                                .child("Stop all"),
                        )
                    }),
            )
            .children(self.forwards.iter().enumerate().map(|(i, f)| {
                let local = f.fwd.local;
                let conns = f.fwd.active.load(Relaxed);
                let error = f.fwd.error.lock().unwrap().clone();
                let name = f.what.split_once('/').map_or(f.what.as_str(), |(_, n)| n).to_string();
                // Sidebar minus paddings, dot, ":port", arrow, stop button and gaps.
                let name = fit_text(window, cx, name, SIDEBAR_W - 140.);
                let detail = format!(
                    "{}:{} · {} · {} · pod {} · {}",
                    f.what,
                    f.remote,
                    f.ctx.name,
                    f.ns,
                    f.pod,
                    error.clone().unwrap_or_else(|| match conns {
                        0 => "idle".into(),
                        1 => "1 connection".into(),
                        n => format!("{n} connections"),
                    })
                );
                div()
                    .id(("fwd-row", i))
                    .group("fwd-row")
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .tooltip(tip(detail))
                    .child(
                        div()
                            .flex_none()
                            .size(px(7.))
                            .rounded_full()
                            .bg(rgb(if error.is_some() { RED } else { GREEN })),
                    )
                    .child(
                        div()
                            .id(("fwd-port", i))
                            .flex_none()
                            .font_family("Menlo")
                            .text_xs()
                            .text_color(rgb(ACCENT))
                            .cursor_pointer()
                            .hover(|s| s.underline())
                            .on_click(move |_, _, cx| cx.open_url(&format!("http://localhost:{local}")))
                            .child(format!(":{local}")),
                    )
                    .child(div().flex_none().text_xs().text_color(rgb(FAINT)).child("→"))
                    .child(div().flex_1().min_w_0().truncate().text_sm().child(name))
                    .child(
                        div()
                            .id(("fwd-stop", i))
                            .flex_none()
                            .invisible()
                            .group_hover("fwd-row", |s| s.visible())
                            .cursor_pointer()
                            .tooltip(tip("Stop"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.forwards.retain(|f| f.fwd.local != local);
                                cx.notify();
                            }))
                            .child(icon("square", RED).size(px(12.))),
                    )
            }))
    }

    /// "Last 15 min" CPU and memory sparklines from the recorded history (nothing until 2 samples exist).
    /// `limit`: a reference line (pod limit / node allocatable) when it's within reach of the data.
    fn trends(
        &self,
        ctx: &Ctx,
        pick: impl FnOnce(&k8s::History) -> Option<std::collections::VecDeque<Usage>>,
        limit: Usage,
    ) -> Option<Div> {
        let series = pick(k8s::HISTORY.lock().unwrap().get(ctx)?)?;
        if series.len() < 2 {
            return None;
        }
        let minutes = (series.len() - 1) * 15 / 60;
        let span = if minutes == 0 {
            format!("last {}s", (series.len() - 1) * 15)
        } else {
            format!("last {minutes} min")
        };
        let chart = |title: &str, values: Vec<f64>, limit: f64, fmt: fn(f64) -> String| {
            let peak = values.iter().cloned().fold(0., f64::max);
            let now = *values.last().unwrap_or(&0.);
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .text_xs()
                        .child(div().text_color(rgb(DIM)).child(format!("{title} · {span}")))
                        .child(div().text_color(rgb(FAINT)).child(format!("now {} · peak {}", fmt(now), fmt(peak)))),
                )
                .child(sparkline(&values, limit))
        };
        Some(
            div()
                .flex()
                .gap_6()
                .child(chart("CPU", series.iter().map(|u| u.cpu).collect(), limit.cpu, fmt_cpu))
                .child(chart("Memory", series.iter().map(|u| u.mem).collect(), limit.mem, fmt_mem)),
        )
    }

    fn pod_usage(&self, r: &Row) -> Option<Usage> {
        Some(self.metrics.get(&r.ctx)?.pods.get(&(r.ns.clone(), r.name.clone()))?.0)
    }

    fn node_usage(&self, r: &Row) -> Option<Usage> {
        self.metrics.get(&r.ctx)?.nodes.get(&r.name).copied()
    }

    /// Rows of another kind in the same context, filtered without cloning the whole slot.
    fn cached(&self, ctx: &Ctx, kind: &str, f: impl Fn(&Row) -> bool) -> Vec<Arc<Row>> {
        let cache = CACHE.lock().unwrap();
        let rows = cache.get(&(ctx.clone(), kind_ix(kind))).into_iter().flat_map(|s| s.rows.values());
        rows.filter(|r| f(r)).cloned().collect()
    }

    /// Link target if that object is in the cache.
    fn link(&self, ctx: &Ctx, kind: usize, ns: &str, name: &str) -> Option<Link> {
        let cache = CACHE.lock().unwrap();
        let exists = cache
            .get(&(ctx.clone(), kind))
            .is_some_and(|s| s.rows.contains_key(&(ns.into(), name.into())));
        exists.then(|| Link::Object(kind, (ctx.clone(), ns.to_string(), name.to_string())))
    }

    /// Pods that belong to / run on / use the given object, if that relation makes sense for its kind.
    fn related_pods(&self, r: &Row) -> Option<Vec<Arc<Row>>> {
        let selector = |path: &str| r.obj.pointer(path).and_then(Value::as_object).filter(|m| !m.is_empty());
        let matching = |sel: &serde_json::Map<String, Value>| {
            self.cached(&r.ctx, "Pods", |p| {
                p.ns == r.ns && sel.iter().all(|(k, v)| p.obj["metadata"]["labels"].get(k) == Some(v))
            })
        };
        let kind = kind_at(self.kind).name;
        Some(match kind {
            "Nodes" => self.cached(&r.ctx, "Pods", |p| k8s::s(&p.obj, "/spec/nodeName") == r.name),
            "Namespaces" => self.cached(&r.ctx, "Pods", |p| p.ns == r.name),
            "Services" => matching(selector("/spec/selector")?),
            "Deployments" | "StatefulSets" | "DaemonSets" | "Jobs" => matching(selector("/spec/selector/matchLabels")?),
            "PVCs" => self.cached(&r.ctx, "Pods", |p| {
                p.ns == r.ns
                    && k8s::arr(&p.obj, "/spec/volumes")
                        .iter()
                        .any(|v| v["persistentVolumeClaim"]["claimName"] == r.name.as_str())
            }),
            "ConfigMaps" | "Secrets" => self.cached(&r.ctx, "Pods", |p| p.ns == r.ns && uses(&p.obj, kind, &r.name)),
            _ => return None,
        })
    }

    fn facts(&self, r: &Row) -> Vec<Fact> {
        let o = &r.obj;
        let s = |p: &str| k8s::s(o, p);
        let images = |p: &str| k8s::arr(o, p).iter().map(|c| k8s::s(c, "/image")).collect::<Vec<_>>().join("\n");
        let plain = |label: &'static str, v: String| (label, v, None);
        let mut f: Vec<Fact> = vec![];
        match kind_at(self.kind).name {
            "Pods" => {
                f.push(plain("Status", r.cells[1].clone()));
                let node = s("/spec/nodeName");
                f.push(("Node", node.clone(), self.link(&r.ctx, kind_ix("Nodes"), "", &node)));
                f.push(plain("Pod IP", s("/status/podIP")));
                f.push(plain("QoS class", s("/status/qosClass")));
                if let Some(owner) = k8s::arr(o, "/metadata/ownerReferences").first() {
                    let (kind, name) = (k8s::s(owner, "/kind"), k8s::s(owner, "/name"));
                    // ReplicaSets aren't listed; link straight to their Deployment.
                    let target = match kind.as_str() {
                        "ReplicaSet" => name.rsplit_once('-').map(|(d, _)| ("Deployments", d.to_string())),
                        "StatefulSet" | "DaemonSet" | "Job" => Some((&kind[..], name.clone())),
                        _ => None,
                    };
                    let link = target.and_then(|(k, n)| {
                        let ix = kinds().iter().position(|x| x.name == k || x.kind == k)?;
                        self.link(&r.ctx, ix, &r.ns, &n)
                    });
                    f.push(("Controlled by", format!("{kind}/{name}"), link));
                }
                f.push(plain("Service account", s("/spec/serviceAccountName")));
            }
            "Nodes" => {
                f.push(plain("Status", r.cells[0].clone()));
                f.push(plain("Roles", r.cells[1].clone()));
                let ip = k8s::arr(o, "/status/addresses")
                    .iter()
                    .find(|a| a["type"] == "InternalIP")
                    .map(|a| k8s::s(a, "/address"));
                f.push(plain("Internal IP", ip.unwrap_or_default()));
                f.push(plain("OS", s("/status/nodeInfo/osImage")));
                f.push(plain("Kernel", s("/status/nodeInfo/kernelVersion")));
                f.push(plain("Runtime", s("/status/nodeInfo/containerRuntimeVersion")));
                f.push(plain("Kubelet", s("/status/nodeInfo/kubeletVersion")));
                f.push(plain("Arch", s("/status/nodeInfo/architecture")));
                let taints: Vec<String> = k8s::arr(o, "/spec/taints")
                    .iter()
                    .map(|t| format!("{}={}:{}", k8s::s(t, "/key"), k8s::s(t, "/value"), k8s::s(t, "/effect")))
                    .collect();
                f.push(plain("Taints", taints.join("\n")));
            }
            "Deployments" => {
                f.push(plain("Replicas", format!("{} ready", r.cells[0])));
                f.push(plain("Strategy", s("/spec/strategy/type")));
                f.push(plain("Images", images("/spec/template/spec/containers")));
            }
            "StatefulSets" => {
                f.push(plain("Replicas", format!("{} ready", r.cells[0])));
                f.push(plain("Service", s("/spec/serviceName")));
                f.push(plain("Images", images("/spec/template/spec/containers")));
            }
            "DaemonSets" => {
                f.push(plain("Pods", format!("{} ready of {} desired", r.cells[1], r.cells[0])));
                f.push(plain("Images", images("/spec/template/spec/containers")));
            }
            "Jobs" => {
                f.push(plain("Completions", r.cells[0].clone()));
                f.push(plain("Status", r.cells[1].clone()));
                if let Some(owner) = k8s::arr(o, "/metadata/ownerReferences").first() {
                    let name = k8s::s(owner, "/name");
                    f.push(("CronJob", name.clone(), self.link(&r.ctx, kind_ix("CronJobs"), &r.ns, &name)));
                }
                f.push(plain("Images", images("/spec/template/spec/containers")));
            }
            "CronJobs" => {
                f.push(plain("Schedule", s("/spec/schedule")));
                f.push(plain("Suspended", s("/spec/suspend")));
                let last = k8s::ts(o, "/status/lastScheduleTime").map(|t| format!("{} ago", k8s::age(t)));
                f.push(plain("Last run", last.unwrap_or_default()));
                f.push(plain("Images", images("/spec/jobTemplate/spec/template/spec/containers")));
            }
            "Services" => {
                f.push(plain("Type", s("/spec/type")));
                f.push(plain("Cluster IP", s("/spec/clusterIP")));
                let ports: Vec<String> = k8s::arr(o, "/spec/ports")
                    .iter()
                    .map(|p| {
                        let node_port = p.get("nodePort").map(|n| format!(" (node {n})")).unwrap_or_default();
                        format!("{} → {}/{}{node_port}", p["port"], k8s::s(p, "/targetPort"), k8s::s(p, "/protocol"))
                    })
                    .collect();
                f.push(plain("Ports", ports.join("\n")));
                let ext: Vec<String> = k8s::arr(o, "/status/loadBalancer/ingress")
                    .iter()
                    .map(|i| format!("{}{}", k8s::s(i, "/ip"), k8s::s(i, "/hostname")))
                    .collect();
                f.push(plain("External", ext.join(", ")));
                let sel = o.pointer("/spec/selector").and_then(Value::as_object);
                let sel: Vec<String> = sel
                    .into_iter()
                    .flatten()
                    .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                    .collect();
                f.push(plain("Selector", sel.join("\n")));
            }
            "Ingresses" => {
                f.push(plain("Class", s("/spec/ingressClassName")));
                // One clickable row per rule; each backend Service links to its page.
                let mut services = vec![];
                for (i, (url, svc, port)) in ingress_routes(o).into_iter().enumerate() {
                    f.push((
                        if i == 0 { "URLs" } else { "" },
                        format!("{url}  →  {svc}:{port}"),
                        Some(Link::Url(url)),
                    ));
                    if !svc.is_empty() && !services.contains(&svc) {
                        services.push(svc);
                    }
                }
                for (i, svc) in services.iter().enumerate() {
                    let link = self.link(&r.ctx, kind_ix("Services"), &r.ns, svc);
                    f.push((if i == 0 { "Services" } else { "" }, svc.clone(), link));
                }
                let tls: Vec<String> = k8s::arr(o, "/spec/tls")
                    .iter()
                    .flat_map(|t| k8s::arr(t, "/hosts"))
                    .filter_map(|h| h.as_str().map(String::from))
                    .collect();
                f.push(plain("TLS", tls.join(", ")));
            }
            "ConfigMaps" | "Secrets" => {
                if kind_at(self.kind).name == "Secrets" {
                    f.push(plain("Type", s("/type")));
                }
            }
            "PVCs" => {
                f.push(plain("Status", s("/status/phase")));
                f.push(plain("Capacity", s("/status/capacity/storage")));
                let modes: Vec<&str> = k8s::arr(o, "/spec/accessModes").iter().filter_map(Value::as_str).collect();
                f.push(plain("Access modes", modes.join(", ")));
                f.push(plain("Storage class", s("/spec/storageClassName")));
                f.push(plain("Volume", s("/spec/volumeName")));
            }
            "Namespaces" => f.push(plain("Status", s("/status/phase"))),
            "Events" => {
                f.push(plain("Type", s("/type")));
                f.push(plain("Reason", s("/reason")));
                let (kind, name, ns) = (s("/involvedObject/kind"), s("/involvedObject/name"), s("/involvedObject/namespace"));
                let link = kinds()
                    .iter()
                    .position(|k| k.kind == kind)
                    .and_then(|ix| self.link(&r.ctx, ix, &ns, &name));
                f.push(("Object", format!("{kind}/{name}"), link));
                f.push(plain("Message", s("/message")));
                f.push(plain("Count", s("/count")));
                f.push(plain("Source", s("/source/component")));
            }
            _ => {}
        }
        if kind_at(self.kind).is_flux() {
            f.extend(self.flux_facts(r));
        }
        if !r.ns.is_empty() {
            f.push(("Namespace", r.ns.clone(), self.link(&r.ctx, kind_ix("Namespaces"), "", &r.ns)));
        }
        let created = k8s::ts(o, "/metadata/creationTimestamp").unwrap_or(r.created);
        f.push(plain("Created", format!("{} ago  ·  {}", k8s::age(created), local_time(created))));
        f.retain(|x| !x.1.is_empty());
        f
    }

    /// What a Flux object points at and what it last applied; sources link to their pages.
    fn flux_facts(&self, r: &Row) -> Vec<Fact> {
        let o = &r.obj;
        let s = |p: &str| k8s::s(o, p);
        let source = |p: &str| -> Fact {
            let (kind, name) = (s(&format!("{p}/kind")), s(&format!("{p}/name")));
            let ns = Some(s(&format!("{p}/namespace")))
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| r.ns.clone());
            let link = k8s::find_kind(|k| k.is_flux() && k.kind == kind).and_then(|ix| self.link(&r.ctx, ix, &ns, &name));
            (
                "Source",
                if name.is_empty() { String::new() } else { format!("{kind}/{name}") },
                link,
            )
        };
        let plain = |label: &'static str, v: String| -> Fact { (label, v, None) };
        let git_ref = ["branch", "tag", "semver", "commit", "name", "digest"]
            .iter()
            .filter_map(|k| Some(format!("{k}: {}", o.pointer(&format!("/spec/ref/{k}"))?.as_str()?)))
            .collect::<Vec<_>>()
            .join(", ");
        let mut f = vec![];
        if suspended(o) {
            f.push(plain("Suspended", "Yes — reconciliation paused".into()));
        }
        match kind_at(self.kind).kind {
            "Kustomization" => {
                f.push(source("/spec/sourceRef"));
                f.push(plain("Path", s("/spec/path")));
                f.push(plain("Revision", s("/status/lastAppliedRevision")));
                f.push(plain("Target ns", s("/spec/targetNamespace")));
                f.push(plain("Prune", s("/spec/prune")));
            }
            "HelmRelease" => {
                let chart = format!("{} {}", s("/spec/chart/spec/chart"), s("/spec/chart/spec/version"));
                f.push(plain("Chart", chart.trim().to_string()));
                f.push(if o.pointer("/spec/chartRef").is_some() {
                    source("/spec/chartRef")
                } else {
                    source("/spec/chart/spec/sourceRef")
                });
                let installed = format!(
                    "{} {} (app {})",
                    s("/status/history/0/chartName"),
                    s("/status/history/0/chartVersion"),
                    s("/status/history/0/appVersion")
                );
                f.push(plain(
                    "Installed",
                    if s("/status/history/0/chartVersion").is_empty() {
                        String::new()
                    } else {
                        installed
                    },
                ));
                f.push(plain("Release", s("/spec/releaseName")));
                f.push(plain("Target ns", s("/spec/targetNamespace")));
            }
            "GitRepository" | "OCIRepository" | "HelmRepository" | "Bucket" => {
                f.push(plain("URL", s("/spec/url")));
                f.push(plain("Ref", git_ref));
                f.push(plain("Type", s("/spec/type")));
                f.push(plain("Revision", s("/status/artifact/revision")));
            }
            "HelmChart" => {
                f.push(plain(
                    "Chart",
                    format!("{} {}", s("/spec/chart"), s("/spec/version")).trim().to_string(),
                ));
                f.push(source("/spec/sourceRef"));
                f.push(plain("Revision", s("/status/artifact/revision")));
            }
            _ => {}
        }
        f.push(plain("Interval", s("/spec/interval")));
        f
    }

    fn events_for(&self, r: &Row) -> Vec<Arc<Row>> {
        let kind = kind_at(self.kind).kind;
        let mut evs = self.cached(&r.ctx, "Events", |e| {
            let io = &e.obj["involvedObject"];
            io["name"] == r.name.as_str() && io["kind"] == kind && k8s::s(io, "/namespace") == r.ns
        });
        evs.sort_by_key(|e| std::cmp::Reverse(e.created));
        evs
    }

    /// Filter (space-separated terms, all must match some column) then sort.
    fn compute_view(&mut self) {
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(String::from).collect();
        let cells: Vec<Vec<String>> = self.rows.iter().map(|r| self.cells(r)).collect();
        let mut view: Vec<usize> = (0..self.rows.len())
            .filter(|&i| {
                let line = cells[i].join(" ").to_lowercase();
                terms.iter().all(|t| line.contains(t))
            })
            .collect();
        if let Some((col, asc)) = self.sort() {
            let age = matches!(self.columns()[col], "Age" | "Last seen");
            let missing = |i: usize| matches!(cells[i][col].as_str(), "" | "–");
            view.sort_by(|&a, &b| {
                // Rows without a value (e.g. no metrics) go last in either direction.
                let o = if age {
                    self.rows[b].created.cmp(&self.rows[a].created)
                } else {
                    natural(&cells[a][col], &cells[b][col])
                };
                missing(a).cmp(&missing(b)).then(if asc { o } else { o.reverse() })
            });
        }
        self.view = view;
    }

    fn on_key(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let cmd = ks.modifiers.platform || ks.modifiers.control;
        if cmd && ks.key == "s" && self.tab == Tab::Yaml {
            self.save_yaml(cx);
            return;
        }
        // Typing inside the YAML editor belongs to the editor, not the table filter.
        if !self.focus.is_focused(window) {
            return;
        }
        // Any key other than select-all/copy drops the selection.
        let all = std::mem::take(&mut self.filter_all);
        match ks.key.as_str() {
            "a" if cmd && self.filter.is_empty() && self.selected.is_none() && !self.problems_mode && !self.helm_mode => {
                self.picked = self
                    .view
                    .iter()
                    .map(|&i| (self.rows[i].ctx.clone(), self.rows[i].ns.clone(), self.rows[i].name.clone()))
                    .collect();
            }
            "a" if cmd => self.filter_all = !self.filter.is_empty(),
            "c" if cmd && all => {
                cx.write_to_clipboard(ClipboardItem::new_string(self.filter.clone()));
                self.filter_all = true;
            }
            "x" if cmd && all => {
                cx.write_to_clipboard(ClipboardItem::new_string(std::mem::take(&mut self.filter)));
            }
            "escape" if self.picker_open => self.picker_open = false,
            "escape" if self.fwd_modal.is_some() => self.fwd_modal = None,
            "escape" if self.ns_open => self.ns_open = false,
            "escape" if self.helm_sel.is_some() => self.helm_sel = None,
            "escape" if !self.picked.is_empty() && self.selected.is_none() => self.picked.clear(),
            "escape" if self.selected.is_some() => self.back(cx),
            "escape" if !self.filter.is_empty() => self.filter.clear(),
            // The filter isn't visible on the detail page, so typing there goes nowhere.
            "backspace" | "v" | "a" | "c" | "x" if self.selected.is_some() => return,
            _ if (self.selected.is_some() || self.helm_sel.is_some()) && !cmd && ks.key != "up" && ks.key != "down" => return,
            "backspace" if cmd || all => self.filter.clear(),
            "backspace" if ks.modifiers.alt => {
                let trimmed = self.filter.trim_end().len();
                let start = self.filter[..trimmed].rfind(char::is_whitespace).map_or(0, |i| i + 1);
                self.filter.truncate(start);
            }
            "backspace" => _ = self.filter.pop(),
            "r" if cmd => self.reconnect(cx),
            "k" if cmd => self.picker_open = !self.picker_open,
            "[" if cmd => self.back(cx),
            "v" if cmd => {
                if let Some(t) = cx.read_from_clipboard().and_then(|c| c.text()) {
                    if all {
                        self.filter.clear();
                    }
                    self.filter.push_str(t.trim());
                }
            }
            "up" | "down" if self.problems_mode || self.helm_mode || self.home_mode => return,
            "up" | "down" => {
                let pos = self.selected.as_ref().and_then(|(c, ns, n)| {
                    self.view.iter().position(|&i| {
                        let r = &self.rows[i];
                        r.ctx == *c && r.ns == *ns && r.name == *n
                    })
                });
                let next = match (pos, ks.key.as_str()) {
                    (None, _) => 0,
                    (Some(p), "up") => p.saturating_sub(1),
                    (Some(p), _) => (p + 1).min(self.view.len().saturating_sub(1)),
                };
                if let Some(&i) = self.view.get(next) {
                    let r = &self.rows[i];
                    self.select((r.ctx.clone(), r.ns.clone(), r.name.clone()), cx);
                    self.list_scroll.scroll_to_item(next, ScrollStrategy::Center);
                }
            }
            _ if !cmd => match &ks.key_char {
                Some(c) if !c.chars().any(char::is_control) => {
                    if all {
                        self.filter.clear();
                    }
                    self.filter.push_str(c);
                }
                _ => return,
            },
            _ => return,
        }
        cx.notify();
    }

    fn ctx_avatar(&self, ctx: &Ctx) -> Div {
        let initial = ctx.name.chars().next().unwrap_or('?').to_ascii_uppercase().to_string();
        div()
            .flex_none()
            .size(px(20.))
            .rounded(px(5.))
            .border_1()
            .border_color(rgb(SURFACE))
            .bg(rgb(self.ctx_color(ctx)))
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.))
            .font_weight(FontWeight::BOLD)
            .text_color(rgb(BG))
            .child(initial)
    }

    /// Compact one-line button at the top of the sidebar; opens the context picker (⌘K).
    fn ctx_switcher(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (title, state, color) = match self.active.as_slice() {
            [] => ("Choose a context".to_string(), "No context selected".to_string(), FAINT),
            [c] => {
                let (state, color) = Self::ctx_state(c);
                (c.name.clone(), state.to_string(), color)
            }
            many => {
                let live = many.iter().filter(|c| Self::ctx_state(c).0 == "Live").count();
                let color = if live == many.len() { GREEN } else { YELLOW };
                (format!("{} clusters", many.len()), format!("{live} of {} live", many.len()), color)
            }
        };
        div()
            .id("ctx-switcher")
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .h(px(34.))
            .px_2()
            .mb_1()
            .rounded_md()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(if self.picker_open { ACCENT } else { BORDER }))
            .cursor_pointer()
            .hover(|s| s.border_color(rgb(0x3a3d50)))
            .tooltip(tip(format!("{state}  ·  ⌘K to switch")))
            .on_click(cx.listener(|this, _, _, cx| {
                this.picker_open = !this.picker_open;
                cx.notify();
            }))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap(px(3.))
                    .children(self.active.iter().take(3).map(|c| self.ctx_avatar(c)))
                    .when(self.active.len() > 3, |d| {
                        d.child(div().text_xs().text_color(rgb(DIM)).child(format!("+{}", self.active.len() - 3)))
                    }),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(if self.active.is_empty() { DIM } else { TEXT }))
                    .child(title),
            )
            .child(div().flex_none().size(px(7.)).rounded_full().bg(rgb(color)))
            .child(icon("chevrons-up-down", FAINT).size(px(13.)))
    }

    /// Popover listing every context from every kubeconfig, with checkboxes.
    fn ctx_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let many_files = self.ctxs.iter().any(|c| c.file != self.ctxs[0].file);
        let items = self.ctxs.iter().enumerate().map(|(i, c)| {
            let on = self.active.contains(c);
            let ctx = c.clone();
            let file = c.file.file_name().unwrap_or_default().to_string_lossy().to_string();
            let file = match file.char_indices().nth(16) {
                Some((i, _)) => format!("{}…", &file[..i]),
                None => file,
            };
            let checkbox = div().flex_none().size(px(16.)).rounded_sm().flex().items_center().justify_center();
            let checkbox = if on {
                checkbox.bg(rgb(self.ctx_color(c))).child(icon("check", BG).size(px(11.)))
            } else {
                checkbox.border_1().border_color(rgb(FAINT))
            };
            div()
                .id(("pick", i))
                .flex()
                .items_center()
                .gap_2p5()
                .px_2()
                .py_1p5()
                .rounded_md()
                .cursor_pointer()
                .hover(|s| s.bg(rgb(HOVER)))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_ctx(ctx.clone(), cx)))
                .child(checkbox)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(rgb(if on { TEXT } else { DIM }))
                        .child(c.name.clone()),
                )
                .when(many_files, |d| {
                    d.child(div().flex_none().text_xs().text_color(rgb(FAINT)).child(file))
                })
                .when(on, |d| {
                    let (state, color) = Self::ctx_state(c);
                    d.child(div().flex_none().text_xs().text_color(rgb(color)).child(state))
                })
        });
        div()
            .id("ctx-picker")
            .absolute()
            .top(px(TOP + 38.))
            .left(px(8.))
            .w(px(340.))
            .max_h(px(480.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_px()
            .p_1p5()
            .rounded_lg()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(0x34374a))
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child("Select one or more contexts")
                    .child(format!("{} of {}", self.active.len(), self.ctxs.len())),
            )
            .children(items)
            .child(div().h_px().my_1().bg(rgb(BORDER)))
            .child(
                div()
                    .id("add")
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .px_2()
                    .py_1p5()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(DIM))
                    .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                    .on_click(cx.listener(|this, _, _, cx| this.add_kubeconfig(cx)))
                    .child(icon("plus", DIM).size(px(16.)))
                    .child("Add kubeconfig file…"),
            )
    }

    fn kind_item(&self, i: usize, k: &'static Kind, indent: bool, cx: &mut Context<Self>) -> AnyElement {
        let on = i == self.kind && !self.problems_mode && !self.helm_mode && !self.home_mode;
        div()
            .id(("kind", i))
            .flex()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .rounded_md()
            .cursor_pointer()
            .text_color(rgb(if on { TEXT } else { DIM }))
            .when(on, |d| d.bg(rgb(SELECT)))
            .when(!on, |d| d.hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT))))
            .on_click(cx.listener(move |this, _, _, cx| this.set_kind(i, cx)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .child(icon(k.icon, if on { ACCENT } else { FAINT }))
                    .child(k.name),
            )
            .when(on && !self.rows.is_empty(), |d| {
                d.child(div().text_xs().text_color(rgb(DIM)).child(self.rows.len().to_string()))
            })
            .when(indent, |d| d.pl_4())
            .into_any_element()
    }

    fn sidebar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let all = kinds();
        let mut kind_items: Vec<AnyElement> = vec![];
        let (errors, warnings) = (
            self.shown_problems().filter(|p| p.severity == 0).count(),
            self.shown_problems().filter(|p| p.severity == 1).count(),
        );
        kind_items.push(
            div()
                .id("home-page")
                .flex()
                .items_center()
                .gap_2p5()
                .mt_2()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(rgb(if self.home_mode { TEXT } else { DIM }))
                .when(self.home_mode, |d| d.bg(rgb(SELECT)))
                .when(!self.home_mode, |d| d.hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT))))
                .on_click(cx.listener(|this, _, _, cx| {
                    (this.home_mode, this.problems_mode, this.helm_mode, this.helm_sel, this.selected) = (true, false, false, None, None);
                    this.history.clear();
                    cx.notify();
                }))
                .child(icon("layout-dashboard", if self.home_mode { ACCENT } else { FAINT }))
                .child("Overview")
                .into_any_element(),
        );
        kind_items.push(
            div()
                .id("problems-page")
                .flex()
                .items_center()
                .justify_between()
                .px_2()
                .py_1()
                .rounded_md()
                .cursor_pointer()
                .text_color(rgb(if self.problems_mode { TEXT } else { DIM }))
                .when(self.problems_mode, |d| d.bg(rgb(SELECT)))
                .when(!self.problems_mode, |d| d.hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT))))
                .on_click(cx.listener(|this, _, _, cx| {
                    (this.problems_mode, this.selected, this.from_problems, this.home_mode) = (true, None, false, false);
                    (this.helm_mode, this.helm_sel) = (false, None);
                    this.history.clear();
                    cx.notify();
                }))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2p5()
                        .child(icon(
                            "triangle-alert",
                            if errors > 0 {
                                RED
                            } else if warnings > 0 {
                                YELLOW
                            } else {
                                FAINT
                            },
                        ))
                        .child("Problems"),
                )
                .when(errors + warnings > 0, |d| {
                    d.child(
                        div()
                            .px_1p5()
                            .rounded_full()
                            .text_xs()
                            .text_color(rgb(BG))
                            .bg(rgb(if errors > 0 { RED } else { YELLOW }))
                            .child((errors + warnings).to_string()),
                    )
                })
                .into_any_element(),
        );
        for (i, k) in all.iter().enumerate().take(k8s::BUILTIN_COUNT) {
            if let Some((label, icon, _)) = GROUPS.iter().find(|(_, _, start)| *start == i) {
                kind_items.push(self.section_header(icon, label, cx));
            }
            if !self.collapsed.contains(section_of(i)) {
                kind_items.push(self.kind_item(i, k, false, cx));
            }
        }
        // CRDs served by any selected context.
        let mut custom: Vec<usize> = self.available.values().flatten().copied().collect();
        custom.sort_unstable();
        custom.dedup();
        kind_items.push(self.section_header("ship", "HELM", cx));
        if !self.collapsed.contains("HELM") {
            let on = self.helm_mode;
            kind_items.push(
                div()
                    .id("helm-page")
                    .flex()
                    .items_center()
                    .gap_2p5()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .text_color(rgb(if on { TEXT } else { DIM }))
                    .when(on, |d| d.bg(rgb(SELECT)))
                    .when(!on, |d| d.hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT))))
                    .on_click(cx.listener(|this, _, _, cx| {
                        (this.helm_mode, this.helm_sel, this.problems_mode, this.selected, this.home_mode) =
                            (true, None, false, None, false);
                        this.history.clear();
                        cx.notify();
                    }))
                    .child(icon("ship", if on { ACCENT } else { FAINT }))
                    .child("Releases")
                    .into_any_element(),
            );
        }
        let (mut flux, other): (Vec<usize>, Vec<usize>) = custom.into_iter().partition(|&i| all[i].is_flux());
        flux.sort_by_key(|&i| (FLUX_ORDER.iter().position(|k| *k == all[i].kind).unwrap_or(usize::MAX), all[i].name));
        if !flux.is_empty() {
            kind_items.push(self.section_header("refresh-cw", "FLUX", cx));
            if !self.collapsed.contains("FLUX") {
                for i in flux {
                    kind_items.push(self.kind_item(i, all[i], false, cx));
                }
            }
        }
        let mut groups: std::collections::BTreeMap<&'static str, Vec<usize>> = Default::default();
        for i in other {
            groups.entry(all[i].group).or_default().push(i);
        }
        if !groups.is_empty() {
            kind_items.push(self.section_header("puzzle", "CUSTOM RESOURCES", cx));
        }
        let custom_open = !self.collapsed.contains("CUSTOM RESOURCES");
        for (gi, (group, mut members)) in groups.into_iter().enumerate().filter(|_| custom_open) {
            members.sort_by_key(|&i| all[i].name);
            let open = self.expanded.contains(group);
            let has_current = members.contains(&self.kind);
            kind_items.push(
                div()
                    .id(("crd-group", gi))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    // Collapsed over the open page: highlight so you can still see where you are.
                    .text_color(rgb(if has_current && !open { ACCENT } else { DIM }))
                    .when(has_current && !open, |d| d.bg(rgb(SELECT)))
                    .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if !this.expanded.remove(group) {
                            this.expanded.insert(group.to_string());
                        }
                        this.save_sidebar();
                        cx.notify();
                    }))
                    .child(icon(if open { "chevron-down" } else { "chevron-right" }, FAINT).size(px(13.)))
                    .child(div().flex_1().min_w_0().truncate().text_sm().child(group))
                    .child(div().text_xs().text_color(rgb(FAINT)).child(members.len().to_string()))
                    .into_any_element(),
            );
            if open {
                for i in members {
                    kind_items.push(self.kind_item(i, all[i], true, cx));
                }
            }
        }
        div()
            .w(px(SIDEBAR_W))
            .flex_none()
            .h_full()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(BORDER))
            .flex()
            .flex_col()
            // Titlebar strip + context switcher stay put; only the resource list scrolls.
            .child(div().h(px(TOP)).flex_none())
            .child(div().px_2().flex_none().child(self.ctx_switcher(cx)))
            .child(
                div()
                    .id("sidebar")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_3()
                    .flex()
                    .flex_col()
                    .gap_px()
                    .children(kind_items),
            )
            .when(!self.forwards.is_empty(), |d| d.child(self.forwards_strip(window, cx)))
    }

    fn table(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cols = self.columns();
        let header = div()
            .flex()
            .items_center()
            .h(px(32.))
            .px_3()
            .border_b_1()
            .border_color(rgb(BORDER))
            .text_xs()
            .font_weight(FontWeight::MEDIUM)
            .text_color(rgb(FAINT))
            .children(self.shown.iter().map(|&i| {
                let h = &cols[i];
                let arrow = match self.sort() {
                    Some((c, asc)) if c == i => {
                        if asc {
                            " ↑"
                        } else {
                            " ↓"
                        }
                    }
                    _ => "",
                };
                let header = h.to_string();
                cell(h, self.flex_w)
                    .id(("hdr", i))
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(TEXT)))
                    .when(!arrow.is_empty(), |d| d.text_color(rgb(DIM)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let asc = !matches!(this.sort(), Some((c, true)) if c == i);
                        this.sorts.insert(kind_at(this.kind).name.to_string(), (header.clone(), asc));
                        if let Err(e) = save_json("sort.json", &this.sorts) {
                            this.status = format!("Could not save sort order: {e}");
                        }
                        cx.notify();
                    }))
                    .child(format!("{h}{arrow}"))
            }));
        let kind = kind_at(self.kind).name.to_lowercase();
        let placeholder = match () {
            _ if self.active.is_empty() => Some("Pick a context from the switcher (top left, ⌘K)".to_string()),
            _ if !self.view.is_empty() => None,
            _ if self.loading => Some(format!("Loading {kind}…")),
            _ if !self.rows.is_empty() => Some(format!("No {kind} match \"{}\"", self.filter)),
            _ if self.errors.is_empty() => Some(format!("No {kind}")),
            _ => Some(String::new()),
        };
        let body = match placeholder {
            Some(text) => div()
                .flex_1()
                .flex()
                .justify_center()
                .pt_16()
                .text_color(rgb(FAINT))
                .child(text)
                .into_any_element(),
            None => uniform_list(
                "rows",
                self.view.len(),
                cx.processor(move |this, range: std::ops::Range<usize>, window, cx| {
                    let cols = this.columns();
                    range
                        .map(|vi| {
                            let r = &this.rows[this.view[vi]];
                            let key = (r.ctx.clone(), r.ns.clone(), r.name.clone());
                            let on = this.selected.as_ref() == Some(&key) || this.picked.contains(&key);
                            let ctx_color = this.ctx_color(&r.ctx);
                            let cur_kind = this.kind;
                            let row_key = key.clone();
                            div()
                                .id(vi)
                                .w_full()
                                .flex()
                                .items_center()
                                .h(px(32.))
                                .px_3()
                                .border_b_1()
                                .border_color(rgb(ROWLINE))
                                .cursor_pointer()
                                .when(on, |d| d.bg(rgb(SELECT)))
                                .when(!on, |d| d.hover(|s| s.bg(rgb(HOVER))))
                                .on_click(cx.listener(move |this, ev: &gpui::ClickEvent, _, cx| this.row_click(&key, ev.modifiers(), cx)))
                                .children(
                                    {
                                        let mut cells: Vec<Option<String>> = this.cells(r).into_iter().map(Some).collect();
                                        this.shown
                                            .iter()
                                            .map(|&i| {
                                                let (v, h) = (cells[i].take().unwrap_or_default(), &cols[i]);
                                                // Room left after a status/context dot; bar cells are short numbers.
                                                let dot = if *h == "Context" || status_color(&v).is_some() { 14. } else { 0. };
                                                let v = match *h {
                                                    "CPU" | "Memory" => v,
                                                    _ => fit_text(window, cx, v, col_width(h).unwrap_or(this.flex_w) - dot),
                                                };
                                                (v, h)
                                            })
                                            .collect::<Vec<_>>()
                                    }
                                    .into_iter()
                                    .map(|(v, h)| match *h {
                                        "Context" => cell(h, this.flex_w)
                                            .flex()
                                            .items_center()
                                            .gap_1p5()
                                            .text_color(rgb(DIM))
                                            .child(div().flex_none().size(px(8.)).rounded_sm().bg(rgb(ctx_color)))
                                            .child(div().truncate().child(v))
                                            .into_any_element(),
                                        "CPU" | "Memory" if cur_kind == kind_ix("Pods") => this.pod_usage_cell(r, h, v, vi),
                                        "CPU" | "Memory" if cur_kind == kind_ix("Nodes") => this.node_usage_cell(r, h, v, vi),
                                        "Ready" if r.cells.get(1).is_some_and(|s| s == "Completed") => {
                                            cell(h, this.flex_w).text_color(rgb(FAINT)).child(v).into_any_element()
                                        }
                                        "Age" | "Last seen" => {
                                            let when =
                                                format!("{} {}", if *h == "Age" { "Created" } else { "Last seen" }, local_time(r.created));
                                            value_cell(h, v, this.flex_w).id((*h, vi)).tooltip(tip(when)).into_any_element()
                                        }
                                        _ => value_cell(h, v, this.flex_w).into_any_element(),
                                    }),
                                )
                                .context_menu({
                                    let (view, key) = (cx.entity().downgrade(), row_key.clone());
                                    let suspended = suspended(&r.obj);
                                    let urls: Vec<String> = if cur_kind == kind_ix("Ingresses") {
                                        let mut urls: Vec<String> = ingress_routes(&r.obj).into_iter().map(|(u, ..)| u).collect();
                                        urls.dedup();
                                        urls
                                    } else {
                                        vec![]
                                    };
                                    move |menu, window, cx| {
                                        row_menu(menu, view.clone(), cur_kind, key.clone(), suspended, urls.clone(), window, cx)
                                    }
                                })
                        })
                        .collect()
                }),
            )
            .track_scroll(self.list_scroll.clone())
            .flex_1()
            .into_any_element(),
        };
        div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .child(header)
            .child(body)
    }

    fn overview(&self, r: &Arc<Row>, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = kind_at(self.kind).name;
        let (mut main, mut side) = (Col::default(), Col::default());
        let mut cond: Option<AnyElement> = None;
        if kind == "Nodes" {
            let (cap, cap_pods) = capacity(&r.obj);
            let a = self.alloc.get(&(r.ctx.clone(), r.name.clone())).copied().unwrap_or_default();
            let u = self.node_usage(r);
            main = main.child(
                card("Utilization")
                    .gap_4()
                    .child(resource_bar(
                        "CPU",
                        format!("of {} cores allocatable", fmt_cpu(cap.cpu)),
                        u.map(|u| u.cpu),
                        a.req.cpu,
                        a.lim.cpu,
                        cap.cpu,
                        fmt_cpu,
                    ))
                    .child(resource_bar(
                        "Memory",
                        format!("of {} allocatable", fmt_mem(cap.mem)),
                        u.map(|u| u.mem),
                        a.req.mem,
                        a.lim.mem,
                        cap.mem,
                        fmt_mem,
                    ))
                    .child(resource_bar(
                        "Pods",
                        format!("of {cap_pods} max"),
                        Some(a.pods as f64),
                        0.,
                        0.,
                        cap_pods,
                        |n| format!("{n:.0}"),
                    ))
                    .children(self.trends(&r.ctx, |h| h.nodes.get(&r.name).cloned(), cap)),
            );
        }
        if kind == "Pods" {
            let u = self.pod_usage(r);
            let (req, lim) = k8s::pod_resources(&r.obj);
            main = main.child(
                card("Resources")
                    .gap_4()
                    .child(pod_resource_bar("CPU", u.map(|u| u.cpu), req.cpu, lim.cpu, fmt_cpu))
                    .child(pod_resource_bar("Memory", u.map(|u| u.mem), req.mem, lim.mem, fmt_mem))
                    .children(self.trends(&r.ctx, |h| h.pods.get(&(r.ns.clone(), r.name.clone())).cloned(), lim)),
            );
        }
        let conditions = k8s::arr(&r.obj, "/status/conditions");
        let details = card("Details").children(self.facts(r).into_iter().enumerate().map(|(i, (label, value, link))| {
            let v = div().id(("fact", i)).flex_1().min_w_0().child(value);
            let v = match link {
                Some(link) => v
                    .text_color(rgb(ACCENT))
                    .cursor_pointer()
                    .hover(|s| s.underline())
                    .on_click(cx.listener(move |this, _, _, cx| match &link {
                        Link::Object(kind, key) => this.goto(*kind, key.clone(), cx),
                        Link::Url(url) => cx.open_url(url),
                    })),
                None => v.text_color(rgb(TEXT)),
            };
            div()
                .flex()
                .gap_3()
                .child(div().flex_none().w(px(110.)).text_color(rgb(FAINT)).child(label))
                .child(v)
        }));
        main = main.child(details);
        if !conditions.is_empty() && kind != "Events" {
            cond = Some(conditions_card(conditions).into_any_element());
        }
        if kind == "Pods" {
            main = main.child(self.containers_card(r, cx));
        }
        if kind == "Secrets" || kind == "ConfigMaps" {
            main = main.child(self.data_card(r, kind == "Secrets", cx));
        }
        if let Some(ports) = self.ports_card(r, cx) {
            main = main.child(ports);
        }
        if let Some(inventory) = self.inventory_card(r, cx) {
            main = main.child(inventory);
        }
        if let Some(pods) = self.related_pods(r) {
            // A node's pods span namespaces; everywhere else they share the object's namespace.
            main = main.child(self.pod_list(pods, kind == "Nodes", window, cx));
        }
        let events = self.events_for(r);
        if !events.is_empty() {
            let total = events.len();
            side = side.child(
                card(format!("Events ({total})"))
                    .children(events.iter().take(5).map(|e| event_row(e, false)))
                    .when(total > 5, |d| {
                        d.child(
                            div()
                                .id("all-events")
                                .text_xs()
                                .text_color(rgb(ACCENT))
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.tab = Tab::Events;
                                    cx.notify();
                                }))
                                .child(format!("All {total} events →")),
                        )
                    }),
            );
        }
        let labels = r.obj.pointer("/metadata/labels").and_then(Value::as_object);
        if let Some(labels) = labels.filter(|l| !l.is_empty()) {
            side = side.child(
                card("Labels").child(div().flex().flex_wrap().gap_1().children(labels.iter().map(|(k, v)| {
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded_sm()
                        .bg(rgb(SURFACE))
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(format!("{k}={}", v.as_str().unwrap_or_default()))
                }))),
            );
        }
        let body = div().id("overview").flex_1().overflow_y_scroll().px_4().py_4();
        if self.two_col() {
            body.child(
                div()
                    .flex()
                    .items_start()
                    .gap_6()
                    .child(div().flex_1().min_w_0().flex().flex_col().gap_6().children(main.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(self.side_w()))
                            .flex()
                            .flex_col()
                            .gap_6()
                            .children(cond)
                            .children(side.0),
                    ),
            )
        } else {
            // One column: custom resources lead with their conditions, everything else ends with them.
            let (first, last) = if kind_at(self.kind).custom { (cond, None) } else { (None, cond) };
            body.flex()
                .flex_col()
                .gap_6()
                .children(first)
                .children(main.0)
                .children(last)
                .children(side.0)
        }
    }

    /// Wide pages split the overview: details left, conditions/events/labels right.
    fn two_col(&self) -> bool {
        self.detail_w >= 1100.
    }

    fn side_w(&self) -> f32 {
        (self.detail_w * 0.38).clamp(340., 520.)
    }

    /// Width available to content in the overview's main column.
    fn main_col_w(&self) -> f32 {
        let inner = self.detail_w - 32.;
        if self.two_col() { inner - self.side_w() - 24. } else { inner }
    }

    /// Keys and values of a ConfigMap/Secret. Secret values stay masked until their eye is clicked.
    fn data_card(&self, r: &Row, secret: bool, cx: &mut Context<Self>) -> Div {
        let entries = data_entries(r, secret);
        let all_shown = !entries.is_empty() && entries.iter().all(|(k, _)| self.revealed.contains(k));
        let keys: Vec<String> = entries.iter().map(|(k, _)| k.clone()).collect();
        let header = div()
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(rgb(FAINT))
                    .child(format!("DATA ({})", entries.len())),
            )
            .when(secret && !entries.is_empty(), |d| {
                d.child(
                    button(
                        "reveal-all",
                        if all_shown { "eye-off" } else { "eye" },
                        if all_shown { "Hide all" } else { "Reveal all" },
                        DIM,
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if all_shown {
                            this.revealed.clear();
                        } else {
                            this.revealed.extend(keys.iter().cloned());
                        }
                        cx.notify();
                    })),
                )
            });
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(header)
            .when(entries.is_empty(), |d| {
                d.child(div().text_xs().text_color(rgb(FAINT)).child("No data"))
            })
            .children(entries.into_iter().enumerate().map(|(i, (key, value))| {
                let shown = !secret || self.revealed.contains(&key);
                let key_for_view = key.clone();
                let toggle_key = key.clone();
                let copy_value = value.clone();
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .font_family("Menlo")
                                    .text_xs()
                                    .text_color(rgb(TEXT))
                                    .child(key),
                            )
                            .when(secret, |d| {
                                d.child(
                                    icon_button(
                                        ("eye", i),
                                        if shown { "eye-off" } else { "eye" },
                                        DIM,
                                        if shown { "Hide value" } else { "Show value" },
                                    )
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !this.revealed.remove(&toggle_key) {
                                            this.revealed.insert(toggle_key.clone());
                                        }
                                        cx.notify();
                                    })),
                                )
                            })
                            .child(icon_button(("copy-value", i), "copy", DIM, "Copy value").on_click(cx.listener(
                                move |this, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(copy_value.clone()));
                                    this.status = "Value copied to clipboard".into();
                                    cx.notify();
                                },
                            ))),
                    )
                    .child({
                        let shell = div()
                            .px_1()
                            .py_0p5()
                            .rounded_md()
                            .bg(rgb(CODE))
                            .border_1()
                            .border_color(rgb(BORDER))
                            .font_family("Menlo")
                            .text_xs();
                        // Shown values are read-only editors: select, copy, scroll, ⌘F. Masked ones stay plain.
                        match self.value_views.get(&key_for_view) {
                            Some(view) if shown => shell.child(Input::new(view).disabled(true).appearance(false).small()),
                            _ => shell.px_2().py_1p5().text_xs().text_color(rgb(FAINT)).child(if shown {
                                if value.is_empty() { "(empty)".to_string() } else { value }
                            } else {
                                "••••••••••••••••".to_string()
                            }),
                        }
                    })
            }))
    }

    fn containers_card(&self, r: &Row, cx: &mut Context<Self>) -> Div {
        let usage = self.metrics.get(&r.ctx).and_then(|m| m.pods.get(&(r.ns.clone(), r.name.clone())));
        let statuses = k8s::arr(&r.obj, "/status/containerStatuses");
        card("Containers").children(k8s::arr(&r.obj, "/spec/containers").iter().map(|c| {
            let name = k8s::s(c, "/name");
            let st = statuses.iter().find(|s| s["name"] == name.as_str());
            let state = st.and_then(|s| s["state"].as_object()).and_then(|m| m.iter().next());
            let (label, color) = match state {
                Some((k, _)) if k == "running" => (
                    "Running".to_string(),
                    if st.is_some_and(|s| s["ready"] == true) { GREEN } else { YELLOW },
                ),
                Some((_, v2)) if v2.get("reason").is_some() => {
                    let reason = k8s::s(v2, "/reason");
                    let color = match reason.as_str() {
                        "Completed" => FAINT,
                        "ContainerCreating" | "PodInitializing" => YELLOW,
                        _ => RED,
                    };
                    (reason, color)
                }
                Some((k, _)) => (k.clone(), FAINT),
                None => ("Unknown".into(), FAINT),
            };
            let restarts = st.and_then(|s| s["restartCount"].as_i64()).unwrap_or(0);
            let last = st
                .and_then(|s| s.pointer("/lastState/terminated"))
                .map(|t| format!("Last terminated: {} (exit {})", k8s::s(t, "/reason"), k8s::s(t, "/exitCode")));
            let (req, lim) = k8s::container_resources(c);
            let used = usage.and_then(|u| u.1.iter().find(|(n, _)| *n == name)).map(|(_, u)| *u);
            let line = |what: &str, u: Option<f64>, rq: f64, lm: f64, fmt: fn(f64) -> String| {
                let opt = |v: f64| if v > 0. { fmt(v) } else { "–".into() };
                format!("{what}  {}   req {}   limit {}", u.map_or("–".into(), fmt), opt(rq), opt(lm))
            };
            div()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .rounded_md()
                .bg(rgb(SURFACE))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(div().flex_none().size(px(7.)).rounded_full().bg(rgb(color)))
                        .child(div().flex_1().truncate().font_weight(FontWeight::MEDIUM).child(name.clone()))
                        .child(div().text_xs().text_color(rgb(color)).child(label))
                        .when(restarts > 0, |d| {
                            d.child(div().text_xs().text_color(rgb(YELLOW)).child(format!("↻ {restarts}")))
                        })
                        .child({
                            let (ctx, ns, pod, container) = (r.ctx.clone(), r.ns.clone(), r.name.clone(), name.clone());
                            button(SharedString::from(format!("shell-{name}")), "square-terminal", "Shell", DIM).on_click(cx.listener(
                                move |this, _, _, cx| {
                                    this.status = open_shell(&ctx, &ns, &pod, &container);
                                    cx.notify();
                                },
                            ))
                        }),
                )
                .child(
                    div()
                        .text_xs()
                        .font_family("Menlo")
                        .text_color(rgb(FAINT))
                        .truncate()
                        .child(k8s::s(c, "/image")),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(line("CPU", used.map(|u| u.cpu), req.cpu, lim.cpu, fmt_cpu)),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(line("Mem", used.map(|u| u.mem), req.mem, lim.mem, fmt_mem)),
                )
                .when_some(last, |d, l| d.child(div().text_xs().text_color(rgb(YELLOW)).child(l)))
        }))
    }

    /// Clickable list of pods, heaviest CPU users first.
    fn pod_list(&self, mut pods: Vec<Arc<Row>>, show_ns: bool, window: &Window, cx: &mut Context<Self>) -> Div {
        let usage = |p: &Row| self.pod_usage(p);
        pods.sort_by(|a, b| {
            let (ua, ub) = (usage(a).map_or(-1., |u| u.cpu), usage(b).map_or(-1., |u| u.cpu));
            ub.total_cmp(&ua).then_with(|| a.name.cmp(&b.name))
        });
        let pods_ix = kind_ix("Pods");
        let header = div()
            .flex()
            .gap_2()
            .px_2()
            .text_xs()
            .text_color(rgb(FAINT))
            .child(div().flex_1().child("Name"))
            .child(div().w(px(64.)).child("CPU"))
            .child(div().w(px(64.)).child("Memory"))
            .child(div().w(px(56.)).child("Restarts"));
        if pods.is_empty() {
            return card("Pods").child(div().text_xs().text_color(rgb(FAINT)).child("Not used by any pod"));
        }
        // Panel padding + CPU/Memory/Restarts columns + status dot; namespace gets its own slice.
        let ns_w = if show_ns { 110. } else { 0. };
        let name_w = self.main_col_w() - 16. - 184. - 14. - 3. * 8. - ns_w;
        card(format!("Pods ({})", pods.len()))
            .child(header)
            .children(pods.iter().enumerate().map(|(i, p)| {
                let u = usage(p);
                let status = p.cells[1].clone();
                let color = status_color(&status).unwrap_or(rgb(FAINT).into());
                let key = (p.ctx.clone(), p.ns.clone(), p.name.clone());
                div()
                    .id(("related", i))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(HOVER)))
                    .on_click(cx.listener(move |this, _, _, cx| this.goto(pods_ix, key.clone(), cx)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .child(fit_text(window, cx, p.name.clone(), name_w)),
                            )
                            .when(show_ns, |d| {
                                // text_xs (12px) is measured against text_sm (14px), so allow 14/12 more.
                                let ns = fit_text(window, cx, p.ns.clone(), (ns_w - 8.) * 14. / 12.);
                                d.child(
                                    div()
                                        .flex_none()
                                        .w(px(ns_w))
                                        .whitespace_nowrap()
                                        .text_xs()
                                        .text_color(rgb(FAINT))
                                        .child(ns),
                                )
                            }),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child(u.map_or("–".into(), |u| fmt_cpu(u.cpu))),
                    )
                    .child(
                        div()
                            .w(px(64.))
                            .text_xs()
                            .text_color(rgb(DIM))
                            .child(u.map_or("–".into(), |u| fmt_mem(u.mem))),
                    )
                    .child(
                        div()
                            .w(px(56.))
                            .text_xs()
                            .text_color(rgb(if p.cells[2] == "0" { DIM } else { YELLOW }))
                            .child(p.cells[2].clone()),
                    )
            }))
    }

    fn yaml_editor(&self, ed: &YamlEditor, dirty: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (hint, hint_color) = match () {
            _ if ed.saving => ("Saving…", ACCENT),
            _ if dirty => ("● Unsaved changes", YELLOW),
            _ => ("Edit and press ⌘S to apply to the cluster", FAINT),
        };
        let notice = |color: u32, bg: u32, text: String| {
            div()
                .mx_3()
                .mt_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(bg))
                .border_1()
                .border_color(rgb(color))
                .text_xs()
                .text_color(rgb(color))
                .child(text)
        };
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .bg(rgb(CODE))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .py_1p5()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .bg(rgb(PANEL))
                    .text_xs()
                    .child(div().flex_1().text_color(rgb(hint_color)).child(hint))
                    .when(dirty || ed.stale, |d| {
                        d.child(button("revert", "rotate-ccw", "Revert", DIM).on_click(cx.listener(|this, _, w, cx| this.reload_yaml(w, cx))))
                    })
                    .child(
                        button("save", "check", "Save  ⌘S", if dirty { TEXT } else { FAINT })
                            .when(dirty, |b| b.bg(rgb(0x3d59a1)).border_color(rgb(ACCENT)))
                            .on_click(cx.listener(|this, _, _, cx| this.save_yaml(cx))),
                    ),
            )
            .when_some(ed.error.clone(), |d, e| d.child(notice(RED, 0x2a1a22, format!("Save failed: {e}"))))
            .when(ed.stale, |d| {
                d.child(
                    notice(YELLOW, 0x2a2618, "This object changed on the cluster since you opened it. Saving will fail with a conflict; Revert loads the latest version (discarding your edits).".into()),
                )
            })
            .child(div().flex_1().min_h_0().child(Input::new(&ed.state).h_full().appearance(false)))
    }

    fn detail(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let kind = kind_at(self.kind);
        let (ctx, ns, name) = self.selected.clone().unwrap_or_default();
        let logs_tab = has_logs(kind.name);
        let tab = |id: &'static str, icon_name: &str, label: &'static str, t: Tab| {
            let on = self.tab == t;
            div()
                .id(id)
                .flex()
                .items_center()
                .gap_1p5()
                .child(icon(icon_name, if on { ACCENT } else { FAINT }).size(px(13.)))
                .px_2p5()
                .py_0p5()
                .rounded_sm()
                .cursor_pointer()
                .text_color(rgb(if on { TEXT } else { DIM }))
                .when(on, |d| d.bg(rgb(SELECT)))
                .when(!on, |d| d.hover(|s| s.text_color(rgb(TEXT))))
                .child(label)
        };
        let singular = kind.kind;
        let editor = self.selected.clone().and_then(|k| self.editors.get(&(self.kind, k)));
        let yaml_dirty = editor.is_some_and(|e| e.dirty(cx));
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(rgb(BORDER))
            .bg(rgb(PANEL))
            .child(
                div()
                    .px_4()
                    .py_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .border_b_1()
                    .border_color(rgb(BORDER))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            .child(
                                icon_button("back", "arrow-left", DIM, "Back (esc / ⌘[)")
                                    .on_click(cx.listener(|this, _, _, cx| this.back(cx))),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child({
                                        // Title row also holds back/copy/actions/close (26px + gap each).
                                        let buttons = 3 + Action::quick(kind, false).len();
                                        let room = self.detail_w - 32. - buttons as f32 * 34.;
                                        // Title is text_base (1rem) vs the table's text_sm, so scale the budget down.
                                        fit_text(window, cx, name, room * 0.875)
                                    }))
                                    .child(div().text_xs().text_color(rgb(FAINT)).truncate().child(format!(
                                        "{singular} · {} · {}",
                                        ctx.name,
                                        if ns.is_empty() { "cluster-scoped" } else { &ns }
                                    ))),
                            )
                            .child(icon_button("copy", "copy", DIM, "Copy").on_click(cx.listener(|this, _, _, cx| {
                                let text = match (this.tab, this.current_editor()) {
                                    (Tab::Yaml, Some(ed)) => ed.state.read(cx).value().to_string(),
                                    (Tab::Logs, _) => this.log_text(),
                                    _ => this.selected_row().map(|r| yaml_text(r)).unwrap_or_default(),
                                };
                                cx.write_to_clipboard(ClipboardItem::new_string(text));
                                this.status = "Copied to clipboard".into();
                                cx.notify();
                            })))
                            .children({
                                // Kind-specific actions, plus Delete for everything.
                                let mut actions = Action::quick(kind, self.selected_row().is_some_and(|r| suspended(&r.obj)));
                                if !actions.contains(&Action::Delete) {
                                    actions.push(Action::Delete);
                                }
                                let (kind_ix, sel) = (self.kind, self.selected.clone().unwrap_or_default());
                                actions
                                    .into_iter()
                                    .enumerate()
                                    .map(|(i, a)| {
                                        let key = sel.clone();
                                        icon_button(("action", i), a.icon(), a.color(), a.label())
                                            .on_click(cx.listener(move |this, _, _, cx| this.act(a, kind_ix, key.clone(), cx)))
                                    })
                                    .collect::<Vec<_>>()
                            }),
                    )
                    .child(
                        div().flex().items_center().gap_1p5().text_xs().child(
                            div()
                                .flex()
                                .p_0p5()
                                .rounded_md()
                                .bg(rgb(SURFACE))
                                .child(tab("overview", "layout-dashboard", "Overview", Tab::Overview).on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.tab = Tab::Overview;
                                        cx.notify();
                                    },
                                )))
                                .child(
                                    tab("yaml", "file-code", if yaml_dirty { "YAML •" } else { "YAML" }, Tab::Yaml).on_click(cx.listener(
                                        |this, _, _, cx| {
                                            this.tab = Tab::Yaml;
                                            cx.notify();
                                        },
                                    )),
                                )
                                .when(has_map(kind.name), |d| {
                                    d.child(tab("map", "map", "Map", Tab::Map).on_click(cx.listener(|this, _, _, cx| {
                                        this.tab = Tab::Map;
                                        cx.notify();
                                    })))
                                })
                                .when(kind.name != "Events", |d| {
                                    d.child(
                                        tab("events", "activity", "Events", Tab::Events).on_click(cx.listener(|this, _, _, cx| {
                                            this.tab = Tab::Events;
                                            cx.notify();
                                        })),
                                    )
                                })
                                .when(logs_tab, |d| {
                                    d.child(
                                        tab("logs", "scroll-text", "Logs", Tab::Logs).on_click(cx.listener(|this, _, _, cx| {
                                            this.tab = Tab::Logs;
                                            cx.notify();
                                        })),
                                    )
                                }),
                        ),
                    ),
            )
            .child(match (self.tab, self.selected_row().cloned()) {
                (Tab::Overview, Some(r)) => self.overview(&r, window, cx).into_any_element(),
                (Tab::Events, Some(r)) => {
                    let events = self.events_for(&r);
                    div()
                        .id("events")
                        .flex_1()
                        .overflow_y_scroll()
                        .px_4()
                        .py_4()
                        .flex()
                        .flex_col()
                        .gap_4()
                        .when(events.is_empty(), |d| {
                            d.child(
                                div()
                                    .text_color(rgb(FAINT))
                                    .child("No events. Kubernetes keeps events for about an hour."),
                            )
                        })
                        .children(events.iter().map(|e| event_row(e, true)))
                        .into_any_element()
                }
                (Tab::Map, Some(r)) => self.map_view(&r, window, cx).into_any_element(),
                (Tab::Overview | Tab::Yaml | Tab::Events | Tab::Map, None) => div()
                    .p_4()
                    .text_color(rgb(FAINT))
                    .child("This object no longer exists.")
                    .into_any_element(),
                (Tab::Yaml, Some(_)) => match editor {
                    Some(ed) => self.yaml_editor(ed, yaml_dirty, cx).into_any_element(),
                    None => div().into_any_element(),
                },
                (Tab::Logs, _) => self.logs_view(cx).into_any_element(),
            })
    }
}

impl Render for Explorer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.refocus) || window.focused(cx).is_none() {
            window.focus(&self.focus);
        }
        if !self.loading && self.pending_open.is_none() && Instant::now() >= self.next_key_at {
            let delay = std::env::var("MASTHEAD_KEY_DELAY").ok().and_then(|d| d.parse().ok()).unwrap_or(0);
            self.next_key_at = Instant::now() + Duration::from_millis(delay);
            if let Some(ms) = self
                .pending_keys
                .front()
                .and_then(|k| k.strip_prefix("wait:"))
                .and_then(|ms| ms.parse().ok())
            {
                self.pending_keys.pop_front();
                self.next_key_at = Instant::now() + Duration::from_millis(ms);
            } else if let Some(key) = self.pending_keys.pop_front() {
                let view = cx.entity().downgrade();
                window.defer(cx, move |window, cx| {
                    if let Ok(ks) = gpui::Keystroke::parse(&key) {
                        window.dispatch_keystroke(ks, cx);
                    }
                    view.update(cx, |_, cx| cx.notify()).ok();
                });
            }
        }
        self.compute_view();
        self.sync_editor(window, cx);
        self.sync_value_views(window, cx);
        self.sync_fwd_modal(window, cx);
        self.sync_ns_search(window, cx);
        self.sync_logs(window, cx);
        self.sync_helm_text(window, cx);
        if let Some((kind, key)) = self.debug_menu.take() {
            // Built after this render: the menu reads our state, which is leased while rendering.
            let view = cx.entity().downgrade();
            window.defer(cx, move |window, cx| {
                let menu = gpui_component::menu::PopupMenu::build(window, cx, |menu, window, cx| {
                    row_menu(menu, view.clone(), kind, key, false, vec![], window, cx)
                });
                view.update(cx, |this, cx| {
                    this.debug_menu_view = Some(menu);
                    cx.notify();
                })
                .ok();
            });
        }
        if let Some(q) = self.palette_query.take() {
            self.open_palette(window, cx);
            if let Some(input) = &self.palette {
                input.update(cx, |s, cx| s.set_value(q, window, cx));
            }
        }
        let main_w = window.viewport_size().width / px(1.) - SIDEBAR_W;
        self.detail_w = main_w;
        let table_w = main_w - 24.;
        let cols = self.columns();
        self.shown = fit_columns(&cols, table_w);
        let fixed: f32 = self.shown.iter().filter_map(|&i| col_width(cols[i])).map(|w| w + COL_GAP).sum();
        let flex_n = self.shown.iter().filter(|&&i| col_width(cols[i]).is_none()).count().max(1) as f32;
        self.flex_w = ((table_w - fixed) / flex_n - COL_GAP).max(MIN_FLEX_W);
        let kind = kind_at(self.kind);
        let summary = if self.home_mode {
            format!("{} clusters", self.active.len())
        } else if self.helm_mode {
            format!("{} releases", self.helm_releases().len())
        } else if self.problems_mode {
            format!("{} problems", self.shown_problems().count())
        } else {
            format!(
                "{} {}{}",
                self.view.len(),
                kind.name.to_lowercase(),
                if self.view.len() == self.rows.len() {
                    String::new()
                } else {
                    format!(" of {}", self.rows.len())
                }
            )
        };
        let search = div()
            .flex_1()
            .max_w(px(520.))
            .h(px(28.))
            .flex()
            .items_center()
            .gap_2()
            .px_2p5()
            .rounded_md()
            .bg(rgb(SURFACE))
            .border_1()
            .border_color(rgb(if self.filter.is_empty() { BORDER } else { ACCENT }))
            .child(icon("search", FAINT).size(px(14.)))
            .child(if self.filter.is_empty() {
                div().text_color(rgb(FAINT)).child("Type anywhere to filter…")
            } else {
                div()
                    .flex()
                    .items_center()
                    .text_color(rgb(TEXT))
                    .child(
                        div()
                            .when(self.filter_all, |d| d.bg(rgb(0x3d59a1)).rounded_sm())
                            .child(self.filter.clone()),
                    )
                    .when(!self.filter_all, |d| d.child(div().w(px(1.5)).h(px(14.)).bg(rgb(ACCENT))))
            });
        let (live_label, live_color) = match () {
            _ if self.active.is_empty() => ("", FAINT),
            _ if self.loading => ("Syncing…", YELLOW),
            _ if !self.errors.is_empty() => ("Degraded", RED),
            _ => ("Live", GREEN),
        };
        let toolbar = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_4()
            .px_4()
            .h(px(TOP))
            .border_b_1()
            .border_color(rgb(BORDER))
            .map(|d| match self.selected.clone() {
                // Detail page: breadcrumb back to the list instead of the (irrelevant) filter.
                Some((_, _, name)) => d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .min_w_0()
                        .child(
                            div()
                                .id("crumb")
                                .text_base()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(rgb(DIM))
                                .cursor_pointer()
                                .hover(|s| s.text_color(rgb(TEXT)))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.history.clear();
                                    this.back(cx);
                                }))
                                .child(kind.name),
                        )
                        .child(icon("chevron-right", FAINT).size(px(14.)))
                        .child(div().text_base().font_weight(FontWeight::SEMIBOLD).truncate().child(name)),
                ),
                None => d
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).child(match () {
                        _ if self.home_mode => "Overview",
                        _ if self.problems_mode => "Problems",
                        _ if self.helm_mode => "Helm releases",
                        _ => kind.name,
                    }))
                    .child(search),
            })
            .child(div().flex_1())
            .child({
                let label = match self.ns_filter.len() {
                    0 => "All namespaces".to_string(),
                    1 => self.ns_filter.iter().next().cloned().unwrap_or_default(),
                    n => format!("{n} namespaces"),
                };
                let active = !self.ns_filter.is_empty();
                button("ns-filter", "folder", label, if active { ACCENT } else { DIM })
                    .when(active || self.ns_open, |b| b.border_color(rgb(ACCENT)))
                    .child(icon("chevron-down", if active { ACCENT } else { FAINT }).size(px(12.)))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.ns_open = !this.ns_open;
                        cx.notify();
                    }))
            })
            .when(!live_label.is_empty(), |d| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .text_xs()
                        .text_color(rgb(live_color))
                        .child(div().size(px(6.)).rounded_full().bg(rgb(live_color)))
                        .child(live_label),
                )
            });
        let errors = self.errors.iter().map(|(c, e)| {
            div()
                .mx_3()
                .mt_2()
                .px_3()
                .py_2()
                .rounded_md()
                .bg(rgb(0x2a1a22))
                .border_1()
                .border_color(rgb(0x4a2433))
                .text_xs()
                .text_color(rgb(RED))
                .child(format!("{}: {e}", c.name))
        });
        let footer = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_3()
            .px_4()
            .h(px(28.))
            .text_xs()
            .text_color(rgb(FAINT))
            .border_t_1()
            .border_color(rgb(BORDER))
            .child(summary)
            .child(div().flex_1().truncate().text_color(rgb(DIM)).child(self.status.clone()))
            .child(
                div()
                    .id("app-stats")
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .tooltip(tip(self.app_stats_tip.clone()))
                    .child(icon("activity", FAINT).size(px(12.)))
                    .child(self.app_stats.clone()),
            )
            // Shortcut hints give way first on narrow windows; the update button never does.
            .child(
                div()
                    .min_w_0()
                    .flex_shrink()
                    .truncate()
                    .child("⌘P search   ↑↓ select   ⌘[ back   esc clear   ⌘K contexts   ⌘R reconnect"),
            )
            .child(match &self.new_release {
                Some(release) => button(
                    "update",
                    "download",
                    if self.updating {
                        "Updating…".to_string()
                    } else {
                        format!("Update to v{}", release.version)
                    },
                    ACCENT,
                )
                .flex_none()
                .border_color(rgb(ACCENT))
                .on_click(cx.listener(|this, _, _, cx| this.install_update(cx)))
                .into_any_element(),
                None => div()
                    .id("version")
                    .cursor_pointer()
                    .hover(|s| s.text_color(rgb(DIM)))
                    .tooltip(tip("Check for updates"))
                    .child(concat!("v", env!("CARGO_PKG_VERSION")))
                    .on_click(cx.listener(|this, _, _, cx| this.check_updates(true, cx)))
                    .into_any_element(),
            });
        div()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key))
            .size_full()
            .flex()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .text_sm()
            .child(
                div()
                    .h_full()
                    .on_mouse_down(MouseButton::Left, Self::focus_root(cx))
                    .child(self.sidebar(window, cx)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(toolbar)
                    .children(errors)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            // A selected object gets the whole page; back returns to the list.
                            .when(self.home_mode, |d| d.child(self.home_view(cx)))
                            .when(self.problems_mode, |d| d.child(self.problems_view(cx)))
                            .when(self.helm_mode, |d| {
                                d.child(if self.helm_sel.is_some() {
                                    self.helm_detail(window, cx)
                                } else {
                                    self.helm_list(window, cx).into_any_element()
                                })
                            })
                            .when(
                                self.selected.is_none() && !self.problems_mode && !self.helm_mode && !self.home_mode,
                                |d| {
                                    d.child(
                                        div()
                                            .flex()
                                            .flex_1()
                                            .min_w_0()
                                            .on_mouse_down(MouseButton::Left, Self::focus_root(cx))
                                            .child(self.table(cx)),
                                    )
                                },
                            )
                            .when(self.selected.is_some(), |d| d.child(self.detail(window, cx))),
                    )
                    .when(
                        !self.picked.is_empty() && self.selected.is_none() && !self.problems_mode && !self.helm_mode && !self.home_mode,
                        |d| d.child(self.bulk_bar(cx)),
                    )
                    .child(footer),
            )
            .when(self.picker_open, |d| {
                d.child(
                    div()
                        .id("backdrop")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.picker_open = false;
                                cx.notify();
                            }),
                        )
                        .child(self.ctx_picker(cx)),
                )
            })
            .when_some(self.debug_menu_view.clone(), |d, menu| {
                d.child(div().absolute().top(px(140.)).left(px(520.)).child(menu))
            })
            .when_some(self.palette.clone(), |d, input| {
                let palette = self.palette_view(&input, cx).into_any_element();
                d.child(
                    div()
                        .id("palette-backdrop")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .occlude()
                        .bg(gpui::hsla(0., 0., 0., 0.35))
                        .flex()
                        .justify_center()
                        .items_start()
                        .pt(px(90.))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.close_palette();
                                cx.notify();
                            }),
                        )
                        .child(palette),
                )
            })
            .when(self.ns_open, |d| {
                d.child(
                    div()
                        .id("ns-backdrop")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .occlude()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.ns_open = false;
                                cx.notify();
                            }),
                        )
                        .child(self.ns_picker(cx)),
                )
            })
            .when(self.fwd_modal.is_some(), |d| {
                let dialog = self.fwd_modal.as_ref().map(|m| self.fwd_dialog(m, cx).into_any_element());
                d.child(
                    div()
                        .id("fwd-modal-backdrop")
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .occlude()
                        .bg(gpui::hsla(0., 0., 0., 0.45))
                        .flex()
                        .items_center()
                        .justify_center()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.fwd_modal = None;
                                cx.notify();
                            }),
                        )
                        .children(dialog),
                )
            })
    }
}

fn section(icon_name: &str, label: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(FAINT))
        .child(icon(icon_name, FAINT).size(px(12.)))
        .child(label)
}

/// `icon_name` may be "" for a text-only button.
fn button(id: impl Into<ElementId>, icon_name: &str, label: impl Into<SharedString>, color: u32) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .gap_1p5()
        .when(!icon_name.is_empty(), |d| d.child(icon(icon_name, color).size(px(13.))))
        .h(px(24.))
        .px_2()
        .rounded_md()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .text_xs()
        .text_color(rgb(color))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(HOVER)).text_color(rgb(TEXT)))
        .child(label.into())
}

/// Square icon-only button with a tooltip (keeps narrow panels from overflowing).
fn icon_button(id: impl Into<ElementId>, icon_name: &str, color: u32, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .size(px(26.))
        .rounded_md()
        .bg(rgb(SURFACE))
        .border_1()
        .border_color(rgb(BORDER))
        .cursor_pointer()
        .hover(|s| s.bg(rgb(HOVER)).border_color(rgb(0x3a3d50)))
        .tooltip(tip(label))
        .child(icon(icon_name, color).size(px(14.)))
}

/// Empty table cell sized for its column; header and rows share this so they line up.
/// Shortens text to `width` with an ellipsis, measured with the table font. GPUI's own `truncate()`
/// doesn't kick in for uniform_list rows (their text is first measured unconstrained, then cached).
fn fit_text(window: &Window, cx: &App, text: String, width: f32) -> String {
    let style = window.text_style();
    let mut runs = vec![style.to_run(text.len())];
    let size = rems(0.875).to_pixels(window.rem_size()); // text_sm
    cx.text_system()
        .line_wrapper(style.font(), size)
        .truncate_line(text.into(), px(width), "…", &mut runs)
        .to_string()
}

/// Every column gets a definite width: GPUI 0.2 caches text size from a first unconstrained
/// measure, so text in a flex-sized cell never gets its "…".
fn cell(header: &str, flex_w: f32) -> Div {
    // Margin, not padding: overflow clips at the padding box, so padding wouldn't keep text apart.
    div()
        .mr(px(COL_GAP))
        .truncate()
        .flex_none()
        .w(px(col_width(header).unwrap_or(flex_w)))
}

const COL_GAP: f32 = 12.;

/// Minimum width of the flexible (Name/Message) columns.
const MIN_FLEX_W: f32 = 180.;

/// Fixed column width, or None for flexible columns.
fn col_width(header: &str) -> Option<f32> {
    Some(match header {
        "Name" | "Message" => return None,
        "Age" | "Restarts" | "Keys" | "Desired" | "Active" | "Suspend" | "Pods" => 76.,
        "Ready" => 92.,
        "Last seen" => 90.,
        "CPU" | "Memory" => 110.,
        "Context" | "Namespace" | "Node" | "Object" | "Hosts" | "Roles" => 160.,
        "Image" => 230.,
        _ => 120.,
    })
}

/// Indices of the columns that fit in `width`, dropping the least important first.
fn fit_columns(cols: &[&str], width: f32) -> Vec<usize> {
    let need = |shown: &[usize]| {
        shown
            .iter()
            .map(|&i| col_width(cols[i]).unwrap_or(MIN_FLEX_W) + COL_GAP)
            .sum::<f32>()
    };
    let mut shown: Vec<usize> = (0..cols.len()).collect();
    for drop in DROP_ORDER {
        if need(&shown) <= width {
            break;
        }
        shown.retain(|&i| cols[i] != *drop);
    }
    // CRD columns aren't in DROP_ORDER: drop them right to left, keeping the essentials.
    while need(&shown) > width {
        let Some(pos) = shown
            .iter()
            .rposition(|&i| !matches!(cols[i], "Name" | "Message" | "Ready" | "Namespace" | "Context" | "Age"))
        else {
            break;
        };
        shown.remove(pos);
    }
    shown
}

fn value_cell(header: &str, v: String, flex_w: f32) -> Div {
    if let Some(p) = v.strip_suffix('%').and_then(|p| p.parse::<f32>().ok()) {
        return cell(header, flex_w)
            .flex()
            .items_center()
            .gap_2()
            .text_color(rgb(DIM))
            .child(bar(p / 100.).w(px(36.)))
            .child(v);
    }
    let warn = match header {
        "Restarts" => v != "0",
        "Ready" => v.split_once('/').is_some_and(|(a, b)| a != b),
        _ => false,
    };
    match status_color(&v) {
        Some(color) => cell(header, flex_w)
            .flex()
            .items_center()
            .gap_1p5()
            .text_color(color)
            .child(div().flex_none().size(px(6.)).rounded_full().bg(color))
            .child(div().truncate().child(v)),
        None if header == "Name" => cell(header, flex_w).text_color(rgb(TEXT)).child(v),
        None => cell(header, flex_w).text_color(rgb(if warn { YELLOW } else { DIM })).child(v),
    }
}

fn status_color(v: &str) -> Option<Hsla> {
    let c = match v {
        "Running" | "Ready" | "Active" | "Bound" | "Normal" | "True" => GREEN,
        "Unknown" | "Suspended" => YELLOW,
        "False" => RED,
        // Finished is fine but not "alive"; grey keeps it apart from running things.
        "Complete" | "Completed" | "Succeeded" => DIM,
        "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" | "Warning" => YELLOW,
        "Failed" | "Error" | "CrashLoopBackOff" | "ImagePullBackOff" | "ErrImagePull" | "OOMKilled" | "NotReady" | "Lost" | "Evicted" => {
            RED
        }
        _ => return None,
    };
    Some(rgb(c).into())
}

/// Numeric-aware compare: "9" < "10", "85Mi" < "1.2Gi", "9%" < "45%".
fn natural(a: &str, b: &str) -> Ordering {
    let num = |s: &str| k8s::quantity(s.trim_end_matches('%'));
    match (num(a), num(b)) {
        (Some(x), Some(y)) => x.total_cmp(&y),
        _ => a.cmp(b),
    }
}

/// One event: reason (yellow for warnings), message, how often and when; `detailed` adds the reporter.
fn event_row(e: &Row, detailed: bool) -> Div {
    let o = &e.obj;
    let warn = k8s::s(o, "/type") == "Warning";
    let count = o["count"]
        .as_i64()
        .filter(|&c| c > 1)
        .map(|c| format!("  ×{c}"))
        .unwrap_or_default();
    let source = [k8s::s(o, "/source/component"), k8s::s(o, "/reportingComponent")]
        .into_iter()
        .find(|s| !s.is_empty());
    div()
        .flex()
        .gap_2p5()
        .child(
            div()
                .mt(px(6.))
                .flex_none()
                .size(px(6.))
                .rounded_full()
                .bg(rgb(if warn { YELLOW } else { FAINT })),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_2()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(rgb(if warn { YELLOW } else { TEXT }))
                                .child(format!("{}{count}", k8s::s(o, "/reason"))),
                        )
                        .when_some(source.filter(|_| detailed), |d, src| {
                            d.child(div().text_xs().text_color(rgb(FAINT)).child(src))
                        })
                        .child(div().flex_1())
                        .child(div().text_xs().text_color(rgb(FAINT)).child(format!("{} ago", k8s::age(e.created)))),
                )
                .child(div().text_xs().text_color(rgb(DIM)).child(k8s::s(o, "/message"))),
        )
}

/// First 3 lines / ~300 characters of a long message (the object's page has the rest).
fn brief(msg: &str) -> String {
    let mut out: String = msg.lines().take(3).collect::<Vec<_>>().join("\n");
    if let Some((cut, _)) = out.char_indices().nth(300) {
        out.truncate(cut);
        out.push('…');
    } else if msg.lines().count() > 3 {
        out.push('…');
    }
    out
}

/// Sources to refresh for `flux reconcile --with-source`: (kind index, namespace, name).
/// Kustomization: its sourceRef. HelmRelease: its chartRef, or its HelmRepository and generated HelmChart.
fn flux_sources(r: &Row) -> Vec<(usize, String, String)> {
    let o = &r.obj;
    let reference = |p: &str| -> Option<(usize, String, String)> {
        let src = o.pointer(p)?;
        let kind = k8s::s(src, "/kind");
        let ns = Some(k8s::s(src, "/namespace"))
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| r.ns.clone());
        let ix = k8s::find_kind(|k| k.is_flux() && k.kind == kind)?;
        Some((ix, ns, k8s::s(src, "/name")))
    };
    let mut out: Vec<_> = ["/spec/sourceRef", "/spec/chartRef", "/spec/chart/spec/sourceRef"]
        .iter()
        .filter_map(|p| reference(p))
        .collect();
    // helm-controller names the chart it generates "<namespace>-<name>" in the source's namespace.
    if o.pointer("/spec/chart/spec").is_some()
        && let (Some(ix), Some((_, ns, _))) = (
            k8s::find_kind(|k| k.is_flux() && k.kind == "HelmChart"),
            reference("/spec/chart/spec/sourceRef"),
        )
    {
        out.push((ix, ns, format!("{}-{}", r.ns, r.name)));
    }
    out
}

/// Bar sparkline, oldest left. Scaled to the peak (so small values still show a shape), unless a
/// limit is within 2× of the peak: then scaled to the limit, drawn as a faint line, and bars color by it.
fn sparkline(values: &[f64], limit: f64) -> Div {
    let peak = values.iter().cloned().fold(0., f64::max);
    let use_limit = limit > 0. && limit < peak * 2.;
    let top = if use_limit { limit.max(peak) } else { peak }.max(f64::EPSILON);
    let height = 40.;
    div()
        .relative()
        .h(px(height))
        .flex()
        .items_end()
        .gap(px(2.))
        .border_b_1()
        .border_color(rgb(BORDER))
        .children(values.iter().map(|&v| {
            let frac = (v / top).clamp(0., 1.) as f32;
            let color = if use_limit { level_color((v / limit) as f32) } else { ACCENT };
            div()
                .flex_1()
                .max_w(px(8.))
                .h(relative(frac.max(0.02)))
                .rounded_t(px(1.5))
                .bg(rgb(color))
        }))
        .when(use_limit, |d| {
            let at = (1. - (limit / top).clamp(0., 1.) as f32) * height;
            d.child(div().absolute().left_0().right_0().top(px(at)).h(px(1.)).bg(rgb(FAINT)))
        })
}

fn checkbox(on: bool) -> Div {
    let b = div().flex_none().size(px(15.)).rounded_sm().flex().items_center().justify_center();
    if on {
        b.bg(rgb(ACCENT)).child(icon("check", BG).size(px(10.)))
    } else {
        b.border_1().border_color(rgb(FAINT))
    }
}

/// Stable identity of a problem (context, kind, object, reason) for "is this new?".
fn problem_id(p: &Problem) -> String {
    format!("{}|{}|{}|{}|{}", p.key.0.name, kind_at(p.kind).kind, p.key.1, p.key.2, p.reason)
}

fn new_problems<'a>(seen: &std::collections::HashSet<String>, current: &[&'a Problem]) -> Vec<&'a Problem> {
    current.iter().copied().filter(|p| !seen.contains(&problem_id(p))).collect()
}

/// Posts a macOS notification via AppleScript: works for a bare binary as well as the .app bundle
/// (UNUserNotificationCenter needs a signed bundle). Runs off the UI thread.
fn notify_macos(title: &str, subtitle: &str, body: &str) {
    let q = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "display notification \"{}\" with title \"{}\" subtitle \"{}\" sound name \"Basso\"",
        q(body),
        q(title),
        q(subtitle)
    );
    std::thread::spawn(move || {
        std::process::Command::new("osascript").args(["-e", &script]).output().ok();
    });
}

/// Collects overview cards for one column (keeps the `.child(...)` builder style).
#[derive(Default)]
struct Col(Vec<AnyElement>);

impl Col {
    fn child(mut self, e: impl IntoElement) -> Self {
        self.0.push(e.into_any_element());
        self
    }
}

/// Status conditions (Ready, Reconciling, Stalled, …) with problem-aware coloring.
fn conditions_card(conditions: &[Value]) -> Div {
    card("Conditions").children(conditions.iter().map(|c| {
        let (ty, status) = (k8s::s(c, "/type"), k8s::s(c, "/status"));
        // For "bad news" types True is the problem; Reconciling/Progressing just means work in progress.
        let negative = matches!(
            ty.as_str(),
            "Stalled" | "Degraded" | "Failed" | "MemoryPressure" | "DiskPressure" | "PIDPressure" | "NetworkUnavailable"
        );
        let busy = matches!(ty.as_str(), "Reconciling" | "Progressing" | "Terminating");
        let color = match (status.as_str(), negative, busy) {
            ("True", true, _) => RED,
            ("True", _, true) => YELLOW,
            ("True", ..) => GREEN,
            ("False", true, _) | ("False", _, true) => FAINT,
            ("False", ..) => RED,
            _ => YELLOW,
        };
        let when = k8s::ts(c, "/lastTransitionTime")
            .map(|t| format!("{} ago", k8s::age(t)))
            .unwrap_or_default();
        let reason = k8s::s(c, "/reason");
        div()
            .flex()
            .gap_2p5()
            .child(div().mt(px(6.)).flex_none().size(px(7.)).rounded_full().bg(rgb(color)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_2()
                            .child(div().font_weight(FontWeight::MEDIUM).child(ty))
                            .child(div().text_xs().text_color(rgb(color)).child(status))
                            .when(!reason.is_empty(), |d| d.child(div().text_xs().text_color(rgb(DIM)).child(reason)))
                            .child(div().flex_1())
                            .child(div().text_xs().text_color(rgb(FAINT)).child(when)),
                    )
                    .child(div().text_xs().text_color(rgb(DIM)).child(k8s::s(c, "/message"))),
            )
    }))
}

fn card(title: impl Into<SharedString>) -> Div {
    div().flex().flex_col().gap_2().child(
        div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(rgb(FAINT))
            .child(title.into().to_uppercase()),
    )
}

fn level_color(frac: f32) -> u32 {
    match frac {
        f if f >= 0.9 => RED,
        f if f >= 0.7 => YELLOW,
        _ => GREEN,
    }
}

/// Thin progress bar; green → yellow → red as it fills. Set the width on the result.
fn bar(frac: f32) -> Div {
    div().flex_none().h(px(5.)).rounded_full().bg(rgb(0x2c2f3d)).child(
        div()
            .h_full()
            .rounded_full()
            .w(relative(frac.clamp(0., 1.)))
            .bg(rgb(level_color(frac))),
    )
}

/// Overlapping fills from the left, each relative to `scale` (draw widest first). Caller sets size.
fn stacked_bar(scale: f64, layers: &[(f64, Hsla)]) -> Div {
    let frac = |v: f64| if scale > 0. { (v / scale) as f32 } else { 0. };
    div()
        .flex_none()
        .relative()
        .rounded_full()
        .bg(rgb(0x2c2f3d))
        .children(layers.iter().filter(|(v, _)| *v > 0.).map(|&(v, color)| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .h_full()
                .rounded_full()
                .w(relative(frac(v).clamp(0., 1.)))
                .bg(color)
        }))
}

fn accent_alpha(a: f32) -> Hsla {
    let mut c: Hsla = rgb(ACCENT).into();
    c.a = a;
    c
}

/// One layered bar per resource, all relative to `total`: limits (faint) and requests (medium)
/// underneath, actual usage (solid, green → red) on top, with an exact-value legend below.
fn resource_bar(title: &str, subtitle: String, used: Option<f64>, req: f64, lim: f64, total: f64, fmt: fn(f64) -> String) -> Div {
    let frac = |v: f64| if total > 0. { (v / total) as f32 } else { 0. };
    let used_color: Hsla = rgb(level_color(frac(used.unwrap_or(0.)))).into();
    let (req_color, lim_color) = (accent_alpha(0.5), accent_alpha(0.2));
    let legend = |color: Hsla, label: &str, v: Option<f64>| {
        let over = v.is_some_and(|v| frac(v) > 1.);
        let text = match v {
            Some(v) => format!(
                "{label} {}  ·  {:.0}%{}",
                fmt(v),
                frac(v) * 100.,
                if over { " (overcommitted)" } else { "" }
            ),
            None => format!("{label}: no metrics"),
        };
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .child(div().flex_none().size(px(8.)).rounded_sm().bg(color))
            .child(div().text_color(rgb(if over { YELLOW } else { DIM })).child(text))
    };
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(
            div()
                .flex()
                .items_baseline()
                .gap_2()
                .child(div().font_weight(FontWeight::MEDIUM).child(title.to_string()))
                .child(div().text_xs().text_color(rgb(FAINT)).child(subtitle)),
        )
        .child({
            // A layer equal to the scale *is* the track; painting it would read as "full".
            let scale_layer = |v: f64| if v == total { 0. } else { v };
            let layers = [
                (scale_layer(lim), lim_color),
                (scale_layer(req), req_color),
                (used.unwrap_or(0.), used_color),
            ];
            stacked_bar(total, &layers).h(px(8.))
        })
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_x_4()
                .gap_y_1()
                .text_xs()
                .child(legend(used_color, "Used", used))
                .when(req > 0., |d| d.child(legend(req_color, "Requested", Some(req))))
                .when(lim > 0., |d| d.child(legend(lim_color, "Limit", Some(lim)))),
        )
}

/// Pod usage scaled to its limit (or request when there's no limit).
fn pod_resource_bar(title: &str, used: Option<f64>, req: f64, lim: f64, fmt: fn(f64) -> String) -> Div {
    match (lim > 0., req > 0.) {
        (true, _) => resource_bar(title, format!("of {} limit", fmt(lim)), used, req, lim, lim, fmt),
        (false, true) => resource_bar(title, format!("of {} request (no limit)", fmt(req)), used, req, 0., req, fmt),
        _ => div()
            .flex()
            .items_baseline()
            .gap_2()
            .child(div().font_weight(FontWeight::MEDIUM).child(title.to_string()))
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(FAINT))
                    .child(format!("{} used  ·  no request or limit set", used.map_or("–".into(), fmt))),
            ),
    }
}

fn capacity(node: &Value) -> (Usage, f64) {
    let q = |p: &str| k8s::quantity(&k8s::s(node, p)).unwrap_or(0.);
    let cap = Usage {
        cpu: q("/status/allocatable/cpu"),
        mem: q("/status/allocatable/memory"),
    };
    (cap, q("/status/allocatable/pods"))
}

fn pct(v: f64, total: f64) -> String {
    if total > 0. {
        format!("{:.0}%", v / total * 100.)
    } else {
        "–".into()
    }
}

fn fmt_cpu(cores: f64) -> String {
    if cores < 1. {
        format!("{:.0}m", cores * 1000.)
    } else {
        format!("{cores:.2}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn fmt_mem(bytes: f64) -> String {
    const MI: f64 = 1048576.;
    match bytes {
        b if b >= 1024. * MI => format!("{:.1}Gi", b / (1024. * MI)),
        b if b >= MI => format!("{:.0}Mi", b / MI),
        b => format!("{:.0}Ki", b / 1024.),
    }
}

/// Does the pod mount or reference this ConfigMap/Secret (volumes, envFrom, env valueFrom)?
fn uses(pod: &Value, kind: &str, name: &str) -> bool {
    let (vol, vol_key, env_from, env_key) = match kind {
        "ConfigMaps" => ("configMap", "name", "configMapRef", "configMapKeyRef"),
        _ => ("secret", "secretName", "secretRef", "secretKeyRef"),
    };
    k8s::arr(pod, "/spec/volumes").iter().any(|v| v[vol][vol_key] == name)
        || k8s::arr(pod, "/spec/containers").iter().any(|c| {
            k8s::arr(c, "/envFrom").iter().any(|e| e[env_from]["name"] == name)
                || k8s::arr(c, "/env").iter().any(|e| e["valueFrom"][env_key]["name"] == name)
        })
}

/// Small JSON settings files in ~/Library/Application Support/Masthead (missing/corrupt -> default).
fn load_json<T: k8s_openapi::serde::de::DeserializeOwned + Default>(name: &str) -> T {
    let path = k8s::app_dir().join(name);
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_json(name: &str, value: &impl k8s_openapi::serde::Serialize) -> anyhow::Result<()> {
    let dir = k8s::app_dir();
    std::fs::create_dir_all(&dir)?;
    Ok(std::fs::write(dir.join(name), serde_json::to_vec_pretty(value)?)?)
}

/// (url, backend service, port) per Ingress path. https when the host is covered by spec.tls (wildcards too).
fn ingress_routes(o: &Value) -> Vec<(String, String, String)> {
    let tls: Vec<String> = k8s::arr(o, "/spec/tls")
        .iter()
        .flat_map(|t| k8s::arr(t, "/hosts"))
        .filter_map(|h| h.as_str().map(String::from))
        .collect();
    let secure = |host: &str| {
        tls.iter().any(|t| {
            t == host
                || t.strip_prefix("*.")
                    .is_some_and(|d| host.split_once('.').is_some_and(|(_, rest)| rest == d))
        })
    };
    // Rules without a host answer on the load balancer's address.
    let lb = k8s::arr(o, "/status/loadBalancer/ingress")
        .first()
        .map(|i| format!("{}{}", k8s::s(i, "/hostname"), k8s::s(i, "/ip")))
        .unwrap_or_default();
    let mut out = vec![];
    for rule in k8s::arr(o, "/spec/rules") {
        let host = Some(k8s::s(rule, "/host")).filter(|h| !h.is_empty()).unwrap_or_else(|| lb.clone());
        if host.is_empty() {
            continue;
        }
        let scheme = if secure(&host) { "https" } else { "http" };
        for p in k8s::arr(rule, "/http/paths") {
            let path = k8s::s(p, "/path");
            let port = &p["backend"]["service"]["port"];
            let port = port
                .get("number")
                .or(port.get("name"))
                .map(|v| v.to_string().replace('"', ""))
                .unwrap_or_default();
            // Regex-style paths (ImplementationSpecific, e.g. "/(.*)") aren't openable; use the host root.
            let path = if path == "/" || path.contains(['(', '*', '[', '$', '^', '?', '+']) {
                ""
            } else {
                &path
            };
            let url = format!("{scheme}://{host}{path}");
            out.push((url, k8s::s(p, "/backend/service/name"), port));
        }
    }
    out
}

/// (key, value) pairs of a ConfigMap/Secret's data and binaryData, decoded where base64.
fn data_entries(r: &Row, secret: bool) -> Vec<(String, String)> {
    let map = |field: &str| r.obj.get(field).and_then(Value::as_object).cloned().unwrap_or_default();
    let decode = |raw: &str, b64: bool| if b64 { decode_secret(raw) } else { raw.to_string() };
    let data = map("data")
        .into_iter()
        .map(|(k, v)| (k, decode(v.as_str().unwrap_or_default(), secret)));
    let binary = map("binaryData")
        .into_iter()
        .map(|(k, v)| (k, decode(v.as_str().unwrap_or_default(), true)));
    data.chain(binary).collect()
}

/// Decodes a base64 Secret value; non-UTF-8 payloads are summarized instead of shown.
fn decode_secret(b64: &str) -> String {
    match serde_json::from_value::<k8s_openapi::ByteString>(Value::String(b64.into())) {
        Ok(bytes) => String::from_utf8(bytes.0).unwrap_or_else(|e| format!("<binary, {} bytes>", e.as_bytes().len())),
        Err(_) => "<invalid base64>".into(),
    }
}

/// (CPU seconds used so far, memory footprint in bytes) of this process.
/// macOS reports the same "Memory" figure Activity Monitor shows; Linux reports RSS.
#[cfg(unix)]
fn self_usage() -> (f64, Option<u64>) {
    // SAFETY: plain syscalls filling zeroed, correctly sized out-structs.
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        let secs = |tv: libc::timeval| tv.tv_sec as f64 + tv.tv_usec as f64 / 1e6;
        #[cfg(target_os = "macos")]
        let mem = {
            let mut info: libc::rusage_info_v2 = std::mem::zeroed();
            let buf = &mut info as *mut _ as *mut libc::rusage_info_t;
            (libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V2, buf) == 0).then_some(info.ri_phys_footprint)
        };
        #[cfg(not(target_os = "macos"))]
        let mem = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|pages| pages * libc::sysconf(libc::_SC_PAGESIZE) as u64);
        (secs(ru.ru_utime) + secs(ru.ru_stime), mem)
    }
}

#[cfg(not(unix))]
fn self_usage() -> (f64, Option<u64>) {
    (0., None)
}

/// Bytes the app's heap actually has allocated right now. Unlike the footprint, this drops as soon as
/// caches are freed (macOS keeps the freed pages reserved for reuse, so the footprint doesn't).
fn heap_in_use() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        // SAFETY: a null zone asks for statistics summed over all malloc zones; stats is a plain out-struct.
        let mut stats: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
        unsafe { libc::malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
        Some(stats.size_in_use as u64)
    }
    #[cfg(not(target_os = "macos"))]
    None
}

/// Saved window frame, relative to the display it was on (identified by a stable UUID).
fn save_window(window: &Window, cx: &App) {
    let Some(display) = window.display(cx).and_then(|d| d.uuid().ok()) else {
        return;
    };
    let (mode, b) = match window.window_bounds() {
        WindowBounds::Windowed(b) => ("windowed", b),
        WindowBounds::Maximized(b) => ("maximized", b),
        WindowBounds::Fullscreen(b) => ("fullscreen", b),
    };
    let f = |p: gpui::Pixels| p / px(1.);
    let saved = serde_json::json!({
        "display": display.to_string(), "mode": mode,
        "x": f(b.origin.x), "y": f(b.origin.y), "w": f(b.size.width), "h": f(b.size.height),
    });
    save_json("window.json", &saved).ok();
}

/// Last frame, if its display is connected and the frame still fits on it.
fn restore_window(cx: &App) -> Option<(WindowBounds, Option<gpui::DisplayId>)> {
    let saved: Value = load_json("window.json");
    let display = cx
        .displays()
        .into_iter()
        .find(|d| d.uuid().is_ok_and(|u| u.to_string() == saved["display"]))?;
    let n = |k: &str| saved[k].as_f64().map(|v| v as f32);
    let (x, y, w, h) = (n("x")?, n("y")?, n("w")?, n("h")?);
    let screen = display.bounds().size;
    let (sw, sh) = (screen.width / px(1.), screen.height / px(1.));
    // Must be on that screen (at least partly visible, not larger than it); otherwise fall back to centered.
    if w > sw || h > sh || x + w < 100. || x > sw - 100. || y < 0. || y > sh - 100. {
        return None;
    }
    let b = Bounds::new(point(px(x), px(y)), size(px(w), px(h)));
    let bounds = match saved["mode"].as_str() {
        Some("maximized") => WindowBounds::Maximized(b),
        Some("fullscreen") => WindowBounds::Fullscreen(b),
        _ => WindowBounds::Windowed(b),
    };
    Some((bounds, Some(display.id())))
}

/// Opens `kubectl exec -it` into a container in a new Ghostty window (Terminal.app if Ghostty isn't
/// installed), so you get your own terminal's fonts, scrollback and copy/paste. Returns a status line.
fn open_shell(ctx: &Ctx, ns: &str, pod: &str, container: &str) -> String {
    let Some(kubectl) = find_in_path("kubectl") else {
        return "Open shell: kubectl not found in PATH".into();
    };
    let shell = "command -v bash >/dev/null 2>&1 && exec bash || exec sh";
    let args: Vec<String> = [
        kubectl.to_string_lossy().as_ref(),
        "--kubeconfig",
        &ctx.file.to_string_lossy(),
        "--context",
        &ctx.name,
        "exec",
        "-it",
        "-n",
        ns,
        pod,
        "-c",
        container,
        "--",
        "sh",
        "-c",
        shell,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    // One shell-quoted command line (both terminals take a line for their shell to run). It names the
    // tab first (OSC 2 title escape), then execs kubectl; wrapped in `sh -c` because Ghostty runs its
    // command as `exec <command>`, which a `;` sequence would break.
    let quote = |a: &str| format!("'{}'", a.replace('\'', "'\\''"));
    let title = format!("{pod}/{container} · {}", ctx.name);
    let inner = format!(
        "printf '\\033]2;%s\\007' {}; exec {}",
        quote(&title),
        args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ")
    );
    let line = format!("sh -c {}", quote(&inner));
    let as_applescript = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
    // Ghostty: a new tab in the running instance (or a window if it has none) via its AppleScript API.
    // Not `open -na Ghostty --args -e`: that starts a second instance, which restores your saved
    // windows and puts the --title on all of them.
    let script = if std::path::Path::new("/Applications/Ghostty.app").exists() {
        format!(
            "tell application \"Ghostty\"\n\
             activate\n\
             set cfg to new surface configuration\n\
             set command of cfg to {}\n\
             set wait after command of cfg to true\n\
             if (count of windows) > 0 then\n\
             new tab in front window with configuration cfg\n\
             else\n\
             new window with configuration cfg\n\
             end if\n\
             end tell",
            as_applescript(&line)
        )
    } else {
        format!(
            "tell application \"Terminal\"\n activate\n do script {}\nend tell",
            as_applescript(&line)
        )
    };
    let res = std::process::Command::new("osascript").args(["-e", &script]).spawn();
    match res {
        Ok(_) => format!("Opening shell in {pod}/{container}…"),
        Err(e) => format!("Open shell failed: {e}"),
    }
}

fn find_in_path(bin: &str) -> Option<std::path::PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Apps started from Finder/Dock get a minimal PATH, so kubectl and kubeconfig exec plugins (doctl,
/// gke-gcloud-auth-plugin, …) wouldn't be found. Take PATH from the login shell instead.
fn use_login_shell_path() {
    if std::env::var_os("TERM").is_some() {
        return; // started from a terminal: PATH is already right
    }
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let Ok(out) = std::process::Command::new(shell).args(["-lc", "printf %s \"$PATH\""]).output() else {
        return;
    };
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.status.success() && !path.is_empty() {
        // SAFETY: called first thing in main, before any other thread exists.
        unsafe { std::env::set_var("PATH", path) };
    }
}

/// Columns of the app map around `r` (same context + namespace), left to right:
/// Ingresses → Services → Workloads → Pods → Config & storage. `self_kind`: the kind of `r`.
fn app_map(self_kind: usize, r: &Row) -> Vec<(&'static str, &'static str, Vec<MapNode>)> {
    use std::collections::BTreeSet;
    let cache = CACHE.lock().unwrap();
    let rows = |kind: &str| -> Vec<Arc<Row>> {
        cache
            .get(&(r.ctx.clone(), kind_ix(kind)))
            .into_iter()
            .flat_map(|s| s.rows.values())
            .filter(|x| x.ns == r.ns)
            .cloned()
            .collect()
    };
    let (ings, svcs, pods) = (rows("Ingresses"), rows("Services"), rows("Pods"));
    let workload_kinds = ["Deployments", "StatefulSets", "DaemonSets", "CronJobs", "Jobs"];
    let wls: Vec<(usize, Arc<Row>)> = workload_kinds
        .iter()
        .flat_map(|k| rows(k).into_iter().map(|x| (kind_ix(k), x)))
        .collect();
    let labels = |v: &Value, p: &str| v.pointer(p).and_then(Value::as_object).cloned().unwrap_or_default();
    let matches = |sel: &serde_json::Map<String, Value>, l: &serde_json::Map<String, Value>| {
        !sel.is_empty() && sel.iter().all(|(k, v)| l.get(k) == Some(v))
    };
    let template = |kind: usize, w: &Row| -> Value {
        match kind_at(kind).name {
            "CronJobs" => w.obj["spec"]["jobTemplate"]["spec"]["template"].clone(),
            _ => w.obj["spec"]["template"].clone(),
        }
    };
    // Workload that owns a pod: ReplicaSet → its Deployment, Job → its CronJob too.
    let owners = |p: &Row| -> Vec<(usize, String)> {
        let Some(o) = k8s::arr(&p.obj, "/metadata/ownerReferences").first() else {
            return vec![];
        };
        let (kind, name) = (k8s::s(o, "/kind"), k8s::s(o, "/name"));
        match kind.as_str() {
            "ReplicaSet" => name
                .rsplit_once('-')
                .map(|(d, _)| vec![(kind_ix("Deployments"), d.to_string())])
                .unwrap_or_default(),
            "StatefulSet" => vec![(kind_ix("StatefulSets"), name)],
            "DaemonSet" => vec![(kind_ix("DaemonSets"), name)],
            "Job" => {
                let mut v = vec![(kind_ix("Jobs"), name.clone())];
                if let Some(job) = wls.iter().find(|(k, w)| *k == kind_ix("Jobs") && w.name == name)
                    && let Some(cj) = k8s::arr(&job.1.obj, "/metadata/ownerReferences")
                        .first()
                        .filter(|o| o["kind"] == "CronJob")
                {
                    v.push((kind_ix("CronJobs"), k8s::s(cj, "/name")));
                }
                v
            }
            _ => vec![],
        }
    };
    let backends = |i: &Row| -> Vec<String> {
        ingress_routes(&i.obj)
            .into_iter()
            .map(|(_, svc, _)| svc)
            .filter(|s| !s.is_empty())
            .collect()
    };

    let (mut ing_set, mut svc_set, mut wl_set, mut pod_set) = (
        BTreeSet::new(),
        BTreeSet::new(),
        BTreeSet::<(usize, String)>::new(),
        BTreeSet::new(),
    );
    let me = kind_at(self_kind).name;
    match me {
        "Ingresses" => {
            ing_set.insert(r.name.clone());
            svc_set.extend(backends(r));
        }
        "Services" => _ = svc_set.insert(r.name.clone()),
        "Pods" => {
            pod_set.insert(r.name.clone());
            wl_set.extend(owners(r));
        }
        _ => _ = wl_set.insert((self_kind, r.name.clone())),
    }
    // Services → pods → owning workloads (and workloads whose template the selector matches, for 0 replicas).
    for s in svcs.iter().filter(|s| svc_set.contains(&s.name)) {
        let sel = labels(&s.obj, "/spec/selector");
        for p in pods.iter().filter(|p| matches(&sel, &labels(&p.obj, "/metadata/labels"))) {
            pod_set.insert(p.name.clone());
            wl_set.extend(owners(p));
        }
        for (k, w) in &wls {
            if matches(&sel, &labels(&template(*k, w), "/metadata/labels")) {
                wl_set.insert((*k, w.name.clone()));
            }
        }
    }
    // Workloads → their pods and the services selecting them.
    for p in &pods {
        if owners(p).iter().any(|o| wl_set.contains(o)) {
            pod_set.insert(p.name.clone());
        }
    }
    for s in &svcs {
        let sel = labels(&s.obj, "/spec/selector");
        let by_wl = wls
            .iter()
            .any(|(k, w)| wl_set.contains(&(*k, w.name.clone())) && matches(&sel, &labels(&template(*k, w), "/metadata/labels")));
        let by_pod = pods
            .iter()
            .any(|p| pod_set.contains(&p.name) && matches(&sel, &labels(&p.obj, "/metadata/labels")));
        if by_wl || by_pod {
            svc_set.insert(s.name.clone());
        }
    }
    // Services → ingresses routing to them.
    for i in &ings {
        if backends(i).iter().any(|b| svc_set.contains(b)) {
            ing_set.insert(i.name.clone());
        }
    }
    // Config & storage referenced by the workload templates (or the pods, without workloads).
    let mut refs: BTreeSet<(&'static str, String)> = BTreeSet::new();
    let mut collect = |spec: &Value| {
        for v in k8s::arr(spec, "/volumes") {
            if let Some(n) = v.pointer("/configMap/name").and_then(Value::as_str) {
                refs.insert(("ConfigMaps", n.into()));
            }
            if let Some(n) = v.pointer("/secret/secretName").and_then(Value::as_str) {
                refs.insert(("Secrets", n.into()));
            }
            if let Some(n) = v.pointer("/persistentVolumeClaim/claimName").and_then(Value::as_str) {
                refs.insert(("PVCs", n.into()));
            }
            for src in k8s::arr(v, "/projected/sources") {
                if let Some(n) = src.pointer("/configMap/name").and_then(Value::as_str) {
                    refs.insert(("ConfigMaps", n.into()));
                }
                if let Some(n) = src.pointer("/secret/name").and_then(Value::as_str) {
                    refs.insert(("Secrets", n.into()));
                }
            }
        }
        for c in k8s::arr(spec, "/containers").iter().chain(k8s::arr(spec, "/initContainers")) {
            for e in k8s::arr(c, "/envFrom") {
                if let Some(n) = e.pointer("/configMapRef/name").and_then(Value::as_str) {
                    refs.insert(("ConfigMaps", n.into()));
                }
                if let Some(n) = e.pointer("/secretRef/name").and_then(Value::as_str) {
                    refs.insert(("Secrets", n.into()));
                }
            }
            for e in k8s::arr(c, "/env") {
                if let Some(n) = e.pointer("/valueFrom/configMapKeyRef/name").and_then(Value::as_str) {
                    refs.insert(("ConfigMaps", n.into()));
                }
                if let Some(n) = e.pointer("/valueFrom/secretKeyRef/name").and_then(Value::as_str) {
                    refs.insert(("Secrets", n.into()));
                }
            }
        }
    };
    let mut from_wl = false;
    for (k, w) in &wls {
        if wl_set.contains(&(*k, w.name.clone())) && kind_at(*k).name != "Jobs" {
            collect(&template(*k, w)["spec"]);
            from_wl = true;
        }
    }
    if !from_wl {
        for p in pods.iter().filter(|p| pod_set.contains(&p.name)) {
            collect(&p.obj["spec"]);
        }
    }
    refs.retain(|(_, n)| !n.starts_with("kube-root-ca") && !n.ends_with("-token"));

    let mut cols = vec![];
    let node = |kind: &str, x: &Row, sub: String, color: u32| MapNode {
        kind: kind_ix(kind),
        name: x.name.clone(),
        sub,
        color,
        missing: false,
    };
    let ing_nodes: Vec<MapNode> = ings
        .iter()
        .filter(|i| ing_set.contains(&i.name))
        .map(|i| {
            let hosts: Vec<String> = ingress_routes(&i.obj)
                .into_iter()
                .map(|(u, ..)| u.split("://").nth(1).unwrap_or(&u).to_string())
                .collect();
            node("Ingresses", i, hosts.join(", "), ACCENT)
        })
        .collect();
    cols.push(("Ingresses", "globe", ing_nodes));
    let mut svc_nodes: Vec<MapNode> = svcs
        .iter()
        .filter(|s| svc_set.contains(&s.name))
        .map(|s| node("Services", s, format!("{} · {}", s.cells[0], s.cells[2]), DIM))
        .collect();
    for missing in svc_set.iter().filter(|n| !svcs.iter().any(|s| &s.name == *n)) {
        svc_nodes.push(MapNode {
            kind: kind_ix("Services"),
            name: missing.clone(),
            sub: "not found".into(),
            color: RED,
            missing: true,
        });
    }
    cols.push(("Services", "share-2", svc_nodes));
    let wl_nodes: Vec<MapNode> = wls
        .iter()
        .filter(|(k, w)| wl_set.contains(&(*k, w.name.clone())))
        .map(|(k, w)| {
            let kind = kind_at(*k);
            let (sub, color) = match kind.name {
                "Deployments" | "StatefulSets" => {
                    let ready = w.cells[0].split_once('/').is_some_and(|(a, b)| a == b);
                    (format!("{} · {} ready", kind.kind, w.cells[0]), if ready { GREEN } else { YELLOW })
                }
                "DaemonSets" => (
                    format!("DaemonSet · {}/{} ready", w.cells[1], w.cells[0]),
                    if w.cells[0] == w.cells[1] { GREEN } else { YELLOW },
                ),
                "Jobs" => (format!("Job · {}", w.cells.get(1).cloned().unwrap_or_default()), DIM),
                _ => (kind.kind.to_string(), DIM),
            };
            MapNode {
                kind: *k,
                name: w.name.clone(),
                sub,
                color,
                missing: false,
            }
        })
        .collect();
    cols.push(("Workloads", "layers", wl_nodes));
    let mut pod_nodes: Vec<MapNode> = pods
        .iter()
        .filter(|p| pod_set.contains(&p.name))
        .map(|p| {
            let color = status_color(&p.cells[1]).map_or(DIM, |_| match p.cells[1].as_str() {
                "Running" => GREEN,
                "Completed" | "Succeeded" => DIM,
                "Pending" | "ContainerCreating" | "PodInitializing" | "Terminating" => YELLOW,
                _ => RED,
            });
            node("Pods", p, format!("{} · {}", p.cells[1], p.cells[0]), color)
        })
        .collect();
    pod_nodes.sort_by(|a, b| a.name.cmp(&b.name));
    cols.push(("Pods", "box", pod_nodes));
    let cfg_nodes: Vec<MapNode> = refs
        .into_iter()
        .map(|(kind, name)| {
            let found = cache
                .get(&(r.ctx.clone(), kind_ix(kind)))
                .and_then(|s| s.rows.get(&(r.ns.clone(), name.clone())).cloned());
            let label = kind_at(kind_ix(kind)).kind;
            match found {
                Some(x) if kind == "PVCs" => MapNode {
                    kind: kind_ix(kind),
                    name,
                    sub: format!("{label} · {} {}", x.cells[0], x.cells[1]),
                    color: if x.cells[0] == "Bound" { GREEN } else { YELLOW },
                    missing: false,
                },
                Some(_) => MapNode {
                    kind: kind_ix(kind),
                    name,
                    sub: label.to_string(),
                    color: DIM,
                    missing: false,
                },
                None => MapNode {
                    kind: kind_ix(kind),
                    name,
                    sub: format!("{label} · not found"),
                    color: RED,
                    missing: true,
                },
            }
        })
        .collect();
    cols.push(("Config & storage", "sliders-horizontal", cfg_nodes));
    cols.retain(|(_, _, nodes)| !nodes.is_empty());
    cols
}

/// kubectl commands for an object, ready to paste: (label, command).
fn kubectl_commands(k: &Kind, key: &Key, shell: Option<&(String, String)>, port: Option<&u16>) -> Vec<(&'static str, String)> {
    let (ctx, ns, name) = key;
    let q = |s: &str| {
        if s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:=@".contains(c)) {
            s.to_string()
        } else {
            format!("'{}'", s.replace('\'', "'\\''"))
        }
    };
    // --kubeconfig only when it isn't the default one.
    let default = dirs::home_dir().map(|h| h.join(".kube/config"));
    let mut base = String::from("kubectl");
    if Some(&ctx.file) != default.as_ref() {
        base += &format!(" --kubeconfig {}", q(&ctx.file.to_string_lossy()));
    }
    base += &format!(" --context {}", q(&ctx.name));
    if k.namespaced {
        base += &format!(" -n {}", q(ns));
    }
    // plural.group is unambiguous for CRDs; built-ins read better short.
    let res = if k.group.is_empty() || !k.custom {
        k.plural.to_string()
    } else {
        format!("{}.{}", k.plural, k.group)
    };
    let obj = format!("{res}/{}", q(name));
    let mut out = vec![
        ("Get YAML", format!("{base} get {obj} -o yaml")),
        ("Describe", format!("{base} describe {obj}")),
    ];
    if has_logs(k.name) {
        out.push(("Follow logs", format!("{base} logs -f {obj} --all-containers")));
    }
    if let Some((pod, container)) = shell {
        out.push(("Shell", format!("{base} exec -it {} -c {} -- sh", q(pod), q(container))));
    }
    if let Some(port) = port {
        out.push(("Port forward", format!("{base} port-forward {obj} {port}:{port}")));
    }
    if k.is_flux() {
        let flux = format!("flux --context {} -n {}", q(&ctx.name), q(ns));
        let fk = k.kind.to_lowercase();
        out.push(("Flux reconcile", format!("{flux} reconcile {fk} {}", q(name))));
    }
    out
}

/// "registry.gitlab.com/org/repo/api:1.2.3" → "api:1.2.3"; digests shortened to 12 hex chars.
/// Severity color for a log line: JSON/logfmt `level`/`severity` (words or pino numbers), klog's
/// `E1011 …` prefix, or a bare ERROR/WARN/INFO/DEBUG word near the start. None when there's no level.
fn log_level(line: &str) -> Option<u32> {
    let word = |w: &str| -> Option<u32> {
        let w = w
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        let pick = |p: &[&str]| p.contains(&w.as_str());
        Some(
            if pick(&["err", "error", "fatal", "panic", "crit", "critical", "emerg", "emergency", "alert"]) {
                RED
            } else if pick(&["warn", "warning"]) {
                YELLOW
            } else if pick(&["info", "notice"]) {
                ACCENT
            } else if pick(&["debug", "trace", "verbose"]) {
                FAINT
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
                    ..30 => FAINT,
                    30..40 => ACCENT,
                    40..50 => YELLOW,
                    _ => RED,
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

fn short_image(image: &str) -> String {
    let (path, digest) = image.split_once('@').map_or((image, None), |(p, d)| (p, Some(d)));
    let name = path.rsplit('/').next().unwrap_or(path);
    match digest {
        Some(d) => format!("{name}@{}", d.trim_start_matches("sha256:").chars().take(12).collect::<String>()),
        None => name.to_string(),
    }
}

/// Local wall-clock time of a Unix timestamp, e.g. "2026-10-11 16:42:05 CEST" (jiff here has no tz data).
fn local_time(secs: i64) -> String {
    // SAFETY: localtime_r only fills the provided struct.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if secs <= 0 || unsafe { libc::localtime_r(&(secs as libc::time_t), &mut tm) }.is_null() {
        return String::new();
    }
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        unsafe { std::ffi::CStr::from_ptr(tm.tm_zone) }.to_string_lossy().into_owned()
    };
    format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02} {zone}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Runs a kube future on the tokio runtime from a GPUI task.
async fn run<T: Send + 'static>(f: impl Future<Output = anyhow::Result<T>> + Send + 'static) -> anyhow::Result<T> {
    RT.spawn(f).await?
}

/// Dock icon. Set at runtime so it also shows for a bare binary (`cargo run`), not just an .app bundle.
#[cfg(target_os = "macos")]
fn set_dock_icon() {
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    static PNG: &[u8] = include_bytes!("../assets/icon.png");
    // SAFETY: standard AppKit calls on the main thread after NSApplication exists; the image lives for the app's lifetime.
    unsafe {
        let data: *mut Object = msg_send![class!(NSData), dataWithBytes: PNG.as_ptr() length: PNG.len()];
        let image: *mut Object = msg_send![class!(NSImage), alloc];
        let image: *mut Object = msg_send![image, initWithData: data];
        let app: *mut Object = msg_send![class!(NSApplication), sharedApplication];
        let () = msg_send![app, setApplicationIconImage: image];
    }
}

/// Opts out of App Nap: macOS otherwise throttles timers of apps whose windows aren't visible, which
/// stalls live updates (and problem notifications) while the app is in the background.
#[cfg(target_os = "macos")]
fn keep_awake() {
    use objc::{class, msg_send, runtime::Object, sel, sel_impl};
    // NSActivityUserInitiatedAllowingIdleSystemSleep: no App Nap, but the Mac may still sleep.
    const OPTIONS: u64 = 0x00FF_FFFF & !(1 << 20);
    // SAFETY: standard Foundation calls; the activity token is retained for the app's lifetime.
    unsafe {
        let info: *mut Object = msg_send![class!(NSProcessInfo), processInfo];
        let reason: *mut Object = msg_send![class!(NSString), stringWithUTF8String: c"Watching Kubernetes clusters".as_ptr()];
        let activity: *mut Object = msg_send![info, beginActivityWithOptions: OPTIONS reason: reason];
        let _: *mut Object = msg_send![activity, retain];
    }
}

fn main() {
    use_login_shell_path();
    #[cfg(target_os = "macos")]
    keep_awake();
    Application::new().with_assets(Assets).run(|cx: &mut App| {
        #[cfg(target_os = "macos")]
        set_dock_icon();
        gpui_component::init(cx);
        LanguageRegistry::singleton().register(
            "yaml",
            &LanguageConfig::new(
                "yaml",
                tree_sitter_yaml::LANGUAGE.into(),
                vec![],
                tree_sitter_yaml::HIGHLIGHTS_QUERY,
                "",
                "",
            ),
        );
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.set_menus(vec![gpui::Menu {
            name: "Masthead".into(),
            items: vec![
                gpui::MenuItem::action("Check for Updates…", CheckForUpdates),
                gpui::MenuItem::separator(),
                gpui::MenuItem::action("Quit Masthead", Quit),
            ],
        }]);
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None), KeyBinding::new("cmd-p", Search, None)]);
        cx.on_window_closed(|cx| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();
        // MASTHEAD_SIZE=WIDTHxHEIGHT forces a centered size and skips saving (used by scripts/screenshot.sh).
        let forced = std::env::var("MASTHEAD_SIZE").ok().and_then(|s| {
            let (w, h) = s.split_once('x')?;
            Some((w.parse().ok()?, h.parse().ok()?))
        });
        let (window_bounds, display_id) = match forced {
            // Same display as your saved window (the one you're looking at), so the capture is live.
            Some((w, h)) => {
                let display = restore_window(cx).and_then(|(_, d)| d);
                (WindowBounds::Windowed(Bounds::centered(display, size(px(w), px(h)), cx)), display)
            }
            None => restore_window(cx).unwrap_or((WindowBounds::Windowed(Bounds::centered(None, size(px(1400.), px(850.)), cx)), None)),
        };
        cx.open_window(
            WindowOptions {
                window_bounds: Some(window_bounds),
                display_id,
                window_min_size: Some(size(px(900.), px(560.))),
                // Screenshot runs (MASTHEAD_SIZE) float above other windows: an occluded window isn't redrawn,
                // so the capture would show a stale frame.
                kind: if forced.is_some() {
                    gpui::WindowKind::Floating
                } else {
                    gpui::WindowKind::Normal
                },
                titlebar: Some(TitlebarOptions {
                    title: Some("Masthead".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(15.))),
                }),
                ..Default::default()
            },
            |window, cx| {
                apply_theme(window, cx);
                // Switch palettes live when macOS switches between light and dark.
                window
                    .observe_window_appearance(|window, cx| {
                        apply_theme(window, cx);
                        window.refresh();
                    })
                    .detach();
                let view = cx.new(Explorer::new);
                window.focus(&view.read(cx).focus);
                // ⌘P from anywhere, even when no element has keyboard focus.
                let (weak, handle) = (view.downgrade(), window.window_handle());
                cx.on_action(move |_: &Search, cx| {
                    // Deferred: the action arrives while the window is busy dispatching the key press,
                    // and updating it from inside that would fail (silently).
                    let weak = weak.clone();
                    cx.defer(move |cx| {
                        handle
                            .update(cx, |_, window, cx| {
                                weak.update(cx, |this, cx| {
                                    if this.palette.is_none() {
                                        this.open_palette(window, cx);
                                    }
                                })
                                .ok();
                            })
                            .ok();
                    });
                });
                let weak = view.downgrade();
                cx.on_action(move |_: &CheckForUpdates, cx| {
                    let weak = weak.clone();
                    cx.defer(move |cx| _ = weak.update(cx, |this, cx| this.check_updates(true, cx)));
                });
                if forced.is_none() {
                    view.update(cx, |_, cx| {
                        cx.observe_window_bounds(window, |_, window, cx| save_window(window, cx)).detach()
                    });
                }
                // gpui-component's Root hosts overlays (editor context menu, search) for the window.
                cx.new(|cx| Root::new(view, window, cx))
            },
        )
        .expect("open window");
        cx.activate(true);
    });
}

#[cfg(all(test, unix))]
#[test]
fn self_usage_is_sane() {
    let (cpu, mem) = self_usage();
    let mem = mem.expect("memory reading");
    assert!((0. ..3600.).contains(&cpu));
    assert!(mem > 1 << 20 && mem < 1 << 36, "{mem}");
    println!("cpu {cpu:.3}s, mem {} MB", mem >> 20);
}

#[test]
fn narrow_tables_drop_minor_columns_first() {
    let cols = ["Namespace", "Name", "Ready", "Status", "Restarts", "Node", "CPU", "Memory", "Age"];
    assert_eq!(fit_columns(&cols, 2000.).len(), cols.len());
    let narrow: Vec<&str> = fit_columns(&cols, 700.).into_iter().map(|i| cols[i]).collect();
    assert!(!narrow.contains(&"Node") && !narrow.contains(&"Restarts"), "{narrow:?}");
    assert!(narrow.contains(&"Name") && narrow.contains(&"Status"));
    assert_eq!(fit_columns(&cols, 0.).into_iter().map(|i| cols[i]).collect::<Vec<_>>(), ["Name"]);
}

#[test]
fn secret_values_decode() {
    assert_eq!(decode_secret("aHVudGVyMg=="), "hunter2");
    assert_eq!(decode_secret(""), "");
    assert_eq!(decode_secret("//79"), "<binary, 3 bytes>");
    assert_eq!(decode_secret("not base64!"), "<invalid base64>");
}

#[test]
fn ingress_urls() {
    let o = serde_json::json!({
        "spec": {
            "tls": [{"hosts": ["*.example.com"]}],
            "rules": [
                {"host": "app.example.com", "http": {"paths": [{"path": "/", "backend": {"service": {"name": "web", "port": {"number": 80}}}}]}},
                {"host": "plain.org", "http": {"paths": [{"path": "/api", "backend": {"service": {"name": "api", "port": {"name": "http"}}}}]}},
                {"host": "docs.example.com", "http": {"paths": [{"path": "/(.*)", "backend": {"service": {"name": "docs", "port": {"number": 80}}}}]}},
                {"http": {"paths": [{"path": "/", "backend": {"service": {"name": "lb", "port": {"number": 8080}}}}]}}
            ]
        },
        "status": {"loadBalancer": {"ingress": [{"ip": "1.2.3.4"}]}}
    });
    let r = ingress_routes(&o);
    assert_eq!(r[0], ("https://app.example.com".into(), "web".into(), "80".into()));
    assert_eq!(r[1], ("http://plain.org/api".into(), "api".into(), "http".into()));
    assert_eq!(r[2].0, "https://docs.example.com");
    assert_eq!(r[3].0, "http://1.2.3.4");
}

/// Deselecting a context must drop everything it cached. `cargo test -- --ignored --nocapture`.
#[cfg(unix)]
#[test]
#[ignore]
fn live_forget_frees_cache() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let mb = || self_usage().1.unwrap_or(0) as f64 / 1048576.;
    let objects = || CACHE.lock().unwrap().values().map(|s| s.rows.len()).sum::<usize>();
    let (ctxs, current, _) = k8s::load_contexts(&k8s::kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("current context");
    let base = mb();
    for round in 1..=3 {
        let mut handles: Vec<_> = (0..k8s::BUILTIN_COUNT).map(|i| k8s::watch(ctx.clone(), i)).collect();
        handles.push(k8s::watch_metrics(ctx.clone()));
        for _ in 0..100 {
            if CACHE.lock().unwrap().values().filter(|s| s.synced).count() == k8s::BUILTIN_COUNT {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let loaded = (objects(), mb());
        println!("  heap in use while loaded: {:.0} MB", heap_in_use().unwrap_or(0) as f64 / 1048576.);
        handles.iter().for_each(|h| h.abort());
        k8s::forget(&ctx);
        std::thread::sleep(Duration::from_secs(3)); // let aborted tasks wind down / late events arrive
        println!("  heap in use: {:.0} MB", heap_in_use().unwrap_or(0) as f64 / 1048576.);
        // Only this context: other live tests may have left their own clusters in the shared cache.
        let slots = CACHE.lock().unwrap().keys().filter(|(c, _)| *c == ctx).count();
        let metrics = usize::from(METRICS.lock().unwrap().contains_key(&ctx));
        println!(
            "round {round}: loaded {} objects ({:.0} MB) -> after forget: {slots} slots, {metrics} metrics, {} objects, {:.0} MB (start {base:.0} MB)",
            loaded.0,
            loaded.1,
            objects(),
            mb()
        );
        assert_eq!((slots, metrics), (0, 0), "stale cache entries survived forget()");
    }
}

/// Reconcile-with-source targets for real Flux objects (server-side dry run). `cargo test -- --ignored`.
#[test]
#[ignore]
fn live_flux_sources_dry_run() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ctxs, current, _) = k8s::load_contexts(&k8s::kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("current context");
    RT.block_on(k8s::discover(ctx.clone())).expect("discover");
    for kind in ["Kustomization", "HelmRelease"] {
        let Some(ix) = k8s::find_kind(|k| k.is_flux() && k.kind == kind) else {
            continue;
        };
        let handle = k8s::watch(ctx.clone(), ix);
        for _ in 0..100 {
            if CACHE.lock().unwrap().get(&(ctx.clone(), ix)).is_some_and(|s| s.synced) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let rows: Vec<Arc<Row>> = CACHE.lock().unwrap()[&(ctx.clone(), ix)].rows.values().take(3).cloned().collect();
        for r in rows {
            let sources = flux_sources(&r);
            println!(
                "{kind} {}/{} -> {:?}",
                r.ns,
                r.name,
                sources
                    .iter()
                    .map(|(i, ns, n)| format!("{}/{ns}/{n}", kind_at(*i).kind))
                    .collect::<Vec<_>>()
            );
            assert!(!sources.is_empty(), "no source found");
            for (six, ns, name) in sources {
                RT.block_on(k8s::patch(
                    ctx.clone(),
                    kind_at(six),
                    ns,
                    name.clone(),
                    k8s::reconcile_patch(),
                    true,
                ))
                .unwrap_or_else(|e| panic!("dry-run reconcile {name}: {e:#}"));
            }
        }
        handle.abort();
    }
}

/// Same startup load as the app (built-ins + metrics + discovery + Flux watches); reports sync timing.
#[test]
#[ignore]
fn live_startup_like_app() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ctxs, current, _) = k8s::load_contexts(&k8s::kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("context");
    let t0 = Instant::now();
    let mut handles: Vec<_> = (0..k8s::BUILTIN_COUNT).map(|i| k8s::watch(ctx.clone(), i)).collect();
    handles.push(k8s::watch_metrics(ctx.clone()));
    let flux: Vec<usize> = RT
        .block_on(k8s::discover(ctx.clone()))
        .expect("discover")
        .into_iter()
        .filter(|&i| kind_at(i).is_flux())
        .collect();
    println!("discovery done at {:?} ({} flux kinds)", t0.elapsed(), flux.len());
    handles.extend(flux.iter().map(|&i| k8s::watch(ctx.clone(), i)));
    let want = k8s::BUILTIN_COUNT + flux.len();
    loop {
        let (synced, errs): (usize, Vec<String>) = {
            let c = CACHE.lock().unwrap();
            (
                c.values().filter(|s| s.synced).count(),
                c.iter()
                    .filter_map(|((_, i), s)| Some(format!("{}: {}", kind_at(*i).name, s.error.clone()?)))
                    .collect(),
            )
        };
        if synced >= want || t0.elapsed() > Duration::from_secs(40) {
            println!("{synced}/{want} synced at {:?}; errors: {errs:?}", t0.elapsed());
            let c = CACHE.lock().unwrap();
            let pending: Vec<&str> = c.iter().filter(|(_, s)| !s.synced).map(|((_, i), _)| kind_at(*i).name).collect();
            println!("not synced: {pending:?}");
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    handles.iter().for_each(|h| h.abort());
}

#[test]
fn only_new_errors_notify() {
    let p = |name: &str, reason: &str| Problem {
        severity: 0,
        kind: kind_ix("Pods"),
        key: (Ctx::default(), "ns".into(), name.into()),
        reason: reason.into(),
        message: String::new(),
        since: 0,
    };
    let (a, b) = (p("a", "CrashLoopBackOff"), p("b", "ImagePullBackOff"));
    let seen: std::collections::HashSet<String> = [problem_id(&a)].into();
    let fresh = new_problems(&seen, &[&a, &b]);
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].key.2, "b");
    // Same object, different failure: that's news too.
    assert_eq!(new_problems(&seen, &[&p("a", "OOMKilled")]).len(), 1);
}

/// Decodes every Helm release Secret of the current context. `cargo test -- --ignored`.
#[test]
#[ignore]
fn live_helm_decode() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ctxs, current, _) = k8s::load_contexts(&k8s::kubeconfig_files());
    let ctx = ctxs
        .into_iter()
        .find(|c| c.name == std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current.clone()).unwrap_or_default())
        .expect("context");
    let secrets = kind_ix("Secrets");
    let handle = k8s::watch(ctx.clone(), secrets);
    for _ in 0..100 {
        if CACHE.lock().unwrap().get(&(ctx.clone(), secrets)).is_some_and(|s| s.synced) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let rows: Vec<Arc<Row>> = CACHE.lock().unwrap()[&(ctx.clone(), secrets)]
        .rows
        .values()
        .filter(|r| r.obj["type"] == "helm.sh/release.v1")
        .cloned()
        .collect();
    assert!(!rows.is_empty(), "no Helm releases on this context");
    let mut ok = 0;
    for r in &rows {
        let v = decode_helm(r, false).unwrap_or_else(|| panic!("failed to decode {}/{}", r.ns, r.name));
        assert!(!k8s::s(&v, "/chart/metadata/name").is_empty());
        assert!(v["chart"].get("templates").is_none(), "templates should be dropped");
        ok += 1;
    }
    let sample = decode_helm(&rows[0], true).unwrap();
    println!(
        "decoded {ok}/{} release secrets; e.g. {} = {} {} ({}), {} bytes manifest, {} bytes notes",
        rows.len(),
        k8s::s(&sample, "/name"),
        k8s::s(&sample, "/chart/metadata/name"),
        k8s::s(&sample, "/chart/metadata/version"),
        k8s::s(&sample, "/info/status"),
        k8s::s(&sample, "/manifest").len(),
        k8s::s(&sample, "/info/notes").len()
    );
    handle.abort();
}

#[test]
fn kubectl_command_shapes() {
    let k = kind_at(kind_ix("Deployments"));
    let ctx = Ctx {
        name: "production".into(),
        file: "/tmp/prod.yaml".into(),
    };
    let cmds = kubectl_commands(
        k,
        &(ctx, "shop".into(), "api".into()),
        Some(&("api-1".into(), "api".into())),
        Some(&3000),
    );
    let get = &cmds.iter().find(|c| c.0 == "Get YAML").unwrap().1;
    assert!(
        get.starts_with("kubectl --kubeconfig /tmp/prod.yaml --context production -n shop get deployments/api -o yaml"),
        "{get}"
    );
    assert!(cmds.iter().any(|c| c.1.ends_with("port-forward deployments/api 3000:3000")));
    assert!(cmds.iter().any(|c| c.1.ends_with("exec -it api-1 -c api -- sh")));
    assert_eq!(
        short_image("registry.gitlab.com/acme/monorepo/api:0.71.0-test.2"),
        "api:0.71.0-test.2"
    );
    assert_eq!(short_image("ghcr.io/x/y@sha256:0123456789abcdef0123"), "y@0123456789ab");
    assert!(local_time(1_800_000_000).starts_with("2027-01-15"));
    assert_eq!(log_level(r#"{"level":30,"msg":"connected"}"#), Some(ACCENT));
    assert_eq!(log_level(r#"{"level":"error","msg":"x"}"#), Some(RED));
    assert_eq!(log_level(r#"{"severity": "WARNING"}"#), Some(YELLOW));
    assert_eq!(log_level("ts=1 level=debug msg=hi"), Some(FAINT));
    assert_eq!(log_level("E1011 12:00:00.000 1 reflector.go:1] boom"), Some(RED));
    assert_eq!(log_level("2026-10-11 12:00:00 WARN disk almost full"), Some(YELLOW));
    assert_eq!(log_level("GET /healthz 200"), None);
    assert_eq!(log_level("fatal: PAYMENTS_TOKEN not set"), Some(RED));
    assert_eq!(log_level("information: none"), None);
    assert_eq!(log_level("Information about errors"), None);
}

/// App map from a real Ingress (MASTHEAD_TEST_CONTEXT, default the current context; MASTHEAD_TEST_INGRESS, default shop as in scripts/demo_cluster.sh).
#[test]
#[ignore]
fn live_app_map() {
    let _serial = crate::LIVE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
    let (ctxs, current, _) = k8s::load_contexts(&k8s::kubeconfig_files());
    let want = std::env::var("MASTHEAD_TEST_CONTEXT").ok().or(current).unwrap_or_default();
    let ctx = ctxs.into_iter().find(|c| c.name == want).expect("context");
    let kinds = [
        "Ingresses",
        "Services",
        "Deployments",
        "StatefulSets",
        "DaemonSets",
        "CronJobs",
        "Jobs",
        "Pods",
        "ConfigMaps",
        "Secrets",
        "PVCs",
    ];
    let handles: Vec<_> = kinds.iter().map(|k| k8s::watch(ctx.clone(), kind_ix(k))).collect();
    for _ in 0..150 {
        let c = CACHE.lock().unwrap();
        if kinds.iter().all(|k| c.get(&(ctx.clone(), kind_ix(k))).is_some_and(|s| s.synced)) {
            break;
        }
        drop(c);
        std::thread::sleep(Duration::from_millis(100));
    }
    let name = std::env::var("MASTHEAD_TEST_INGRESS").unwrap_or("shop".into());
    let ing = CACHE.lock().unwrap()[&(ctx.clone(), kind_ix("Ingresses"))]
        .rows
        .values()
        .find(|r| r.name == name)
        .cloned()
        .expect("ingress");
    let cols = app_map(kind_ix("Ingresses"), &ing);
    for (title, _, nodes) in &cols {
        let names: Vec<String> = nodes
            .iter()
            .map(|n| format!("{}{}", n.name, if n.missing { " (MISSING)" } else { "" }))
            .collect();
        println!("{title:<18} {names:?}");
    }
    let col = |t: &str| cols.iter().find(|c| c.0 == t).map(|c| c.2.len()).unwrap_or(0);
    assert!(col("Services") > 0 && col("Workloads") > 0 && col("Pods") > 0, "chain incomplete");
    // Starting from the Deployment must reach the same Ingress.
    let wl = cols.iter().find(|c| c.0 == "Workloads").unwrap().2.first().unwrap();
    let dep = CACHE.lock().unwrap()[&(ctx.clone(), wl.kind)]
        .rows
        .values()
        .find(|r| r.name == wl.name)
        .cloned()
        .unwrap();
    let back = app_map(wl.kind, &dep);
    assert!(
        back.iter().any(|c| c.0 == "Ingresses" && c.2.iter().any(|n| n.name == name)),
        "reverse path missing the ingress"
    );
    handles.iter().for_each(|h| h.abort());
    k8s::forget(&ctx);
}
