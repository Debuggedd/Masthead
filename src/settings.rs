//! The Settings page (⌘,): preferences saved in settings.json, plus per-context and sidebar options.

use super::*;
use std::sync::LazyLock;

/// Settings page values, saved in settings.json. Fields missing from the file take their defaults, so
/// older files (and newly added settings) just work.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
#[serde(default)]
pub struct Prefs {
    /// "system", "light" or "dark".
    pub theme: String,
    pub auto_update: bool,
    pub notify: bool,
    /// Notify about warnings too, not just errors.
    pub notify_warnings: bool,
    pub notify_sound: bool,
    /// Namespaces never notified about: comma-separated, `*` wildcards (e.g. "dev-*, kube-system").
    pub notify_ignore: String,
    /// How long a Flux object may be not Ready before it's a problem.
    pub flux_grace_min: i64,
    /// "auto" (Ghostty if installed, else Terminal), "ghostty", "terminal" or "iterm".
    pub terminal: String,
    pub debug_image: String,
    pub log_wrap: bool,
    pub log_timestamps: bool,
    pub log_since: Option<i64>,
    pub open_browser: bool,
    /// Ask before deleting or redeploying.
    pub confirm: bool,
    /// Built-in kinds hidden from the sidebar (by name).
    pub hidden_kinds: std::collections::BTreeSet<String>,
    /// Per context name: display name, color, notifications muted.
    pub contexts: std::collections::BTreeMap<String, CtxPrefs>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
#[serde(default)]
pub struct CtxPrefs {
    pub alias: String,
    pub color: Option<u32>,
    pub mute: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            auto_update: true,
            notify: true,
            notify_warnings: false,
            notify_sound: true,
            notify_ignore: String::new(),
            flux_grace_min: 5,
            terminal: "auto".into(),
            debug_image: "busybox:1.36".into(),
            log_wrap: false,
            log_timestamps: false,
            log_since: None,
            open_browser: false,
            confirm: true,
            hidden_kinds: Default::default(),
            contexts: Default::default(),
        }
    }
}

/// A copy for code outside `Explorer` (terminal launching, notifications); kept in sync by `save_settings`.
pub static SHARED: LazyLock<std::sync::RwLock<Prefs>> = LazyLock::new(|| std::sync::RwLock::new(load_json("settings.json")));

pub fn shared() -> Prefs {
    SHARED.read().unwrap().clone()
}

/// `*` wildcards only, e.g. "dev-*" or "*-test".
pub fn glob_match(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((head, rest)) => {
            text.starts_with(head) && (head.len()..=text.len()).any(|i| text.is_char_boundary(i) && glob_match(rest, &text[i..]))
        }
    }
}

/// Theme override: 0 follows macOS, 1 light, 2 dark (read by `apply_theme`).
pub static THEME: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

pub fn theme_code(theme: &str) -> u8 {
    match theme {
        "light" => 1,
        "dark" => 2,
        _ => 0,
    }
}

/// Context colors to pick from (None: by position, as before).
const PICKABLE: [Option<u32>; 7] = [
    None,
    Some(0x7aa2f7),
    Some(0xbb9af7),
    Some(0x7dcfff),
    Some(0x73daca),
    Some(0xe0af68),
    Some(0xf7768e),
];

const SHORTCUTS: &[(&str, &str)] = &[
    ("⌘P", "Search everything"),
    ("⌘K", "Contexts"),
    ("⌘,", "Settings"),
    ("type anywhere", "Filter the current page"),
    ("↑ ↓ / Enter", "Select / open"),
    ("Esc, ⌘[", "Back, clear filter or selection"),
    ("⌘-click, ⇧-click, ⌘A", "Pick rows for bulk actions"),
    ("⌘S", "Save in the YAML editor"),
    ("⌘R", "Reconnect"),
];

fn row(label: &'static str, hint: impl Into<SharedString>, control: impl IntoElement) -> Div {
    let hint = hint.into();
    div()
        .flex()
        .items_center()
        .gap_6()
        .py_2()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(div().text_sm().text_color(rgb(TEXT)).child(label))
                .when(!hint.is_empty(), |d| d.child(div().text_xs().text_color(rgb(FAINT)).child(hint))),
        )
        .child(div().flex_none().child(control))
}

fn switch(id: impl Into<ElementId>, on: bool) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .when(on, |d| d.justify_end())
        .w(px(32.))
        .h(px(18.))
        .p(px(2.))
        .rounded_full()
        .cursor_pointer()
        .bg(rgb(if on { ACCENT } else { 0x3a3d50 }))
        .child(div().size(px(14.)).rounded_full().bg(gpui::white()))
}

fn panel(icon_name: &'static str, title: &'static str) -> Div {
    div()
        .flex()
        .flex_none()
        .flex_col()
        .px_5()
        .py_4()
        .rounded_lg()
        .bg(rgb(PANEL))
        .border_1()
        .border_color(rgb(BORDER))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .pb_2()
                .child(icon(icon_name, ACCENT).size(px(15.)))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(title)),
        )
}

impl Explorer {
    /// Display name for a context: its alias from Settings, else its kubeconfig name.
    pub fn ctx_label(&self, ctx: &Ctx) -> String {
        match self.prefs.contexts.get(&ctx.name) {
            Some(p) if !p.alias.trim().is_empty() => p.alias.trim().to_string(),
            _ => ctx.name.clone(),
        }
    }

    /// Notifications are off for this problem's context or namespace.
    pub fn muted(&self, ctx: &Ctx, ns: &str) -> bool {
        self.prefs.contexts.get(&ctx.name).is_some_and(|p| p.mute)
            || self
                .prefs
                .notify_ignore
                .split(',')
                .map(str::trim)
                .any(|g| !g.is_empty() && !ns.is_empty() && glob_match(g, ns))
    }

    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        self.settings_mode = true;
        self.reset_armed = false;
        cx.notify();
    }

    fn ctx_prefs(&mut self, name: &str) -> &mut CtxPrefs {
        self.prefs.contexts.entry(name.to_string()).or_default()
    }

    /// Text fields need the window, so they're made on first render of the page.
    pub fn sync_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.settings_mode {
            return;
        }
        let mut fields: Vec<(String, String, &'static str)> = vec![
            ("ignore".into(), self.prefs.notify_ignore.clone(), "e.g. dev-*, kube-system"),
            ("grace".into(), self.prefs.flux_grace_min.to_string(), "minutes"),
            ("image".into(), self.prefs.debug_image.clone(), "busybox:1.36"),
        ];
        for c in &self.ctxs {
            let alias = self.prefs.contexts.get(&c.name).map(|p| p.alias.clone()).unwrap_or_default();
            fields.push((format!("alias:{}", c.name), alias, "Display name"));
        }
        for (key, value, placeholder) in fields {
            if self.settings_inputs.contains_key(&key) {
                continue;
            }
            let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value));
            let k = key.clone();
            cx.subscribe(&input, move |this: &mut Self, input, ev: &InputEvent, cx| {
                if matches!(ev, InputEvent::Change) {
                    let v = input.read(cx).value().to_string();
                    this.apply_field(&k, v);
                    this.save_settings();
                    cx.notify();
                }
            })
            .detach();
            self.settings_inputs.insert(key, input);
        }
    }

    fn apply_field(&mut self, key: &str, v: String) {
        match key {
            "ignore" => self.prefs.notify_ignore = v,
            "grace" => {
                if let Ok(n) = v.trim().parse::<i64>() {
                    self.prefs.flux_grace_min = n.clamp(0, 1440);
                }
            }
            "image" => {
                self.prefs.debug_image = if v.trim().is_empty() {
                    Prefs::default().debug_image
                } else {
                    v.trim().into()
                }
            }
            _ => {
                if let Some(name) = key.strip_prefix("alias:") {
                    self.ctx_prefs(name).alias = v;
                }
            }
        }
    }

    fn field(&self, key: &str, width: f32) -> Div {
        div().w(px(width)).when_some(self.settings_inputs.get(key).cloned(), |d, input| {
            d.child(Input::new(&input).small())
        })
    }

    /// One chip per choice, the current one highlighted.
    fn choice<T: PartialEq + Clone + 'static>(
        &self,
        id: &'static str,
        options: &[(&'static str, T)],
        current: &T,
        pick: impl Fn(&mut Self, T, &mut Window, &mut Context<Self>) + Clone + 'static,
        cx: &mut Context<Self>,
    ) -> Div {
        div()
            .flex()
            .p_0p5()
            .rounded_md()
            .bg(rgb(SURFACE))
            .text_xs()
            .children(options.iter().enumerate().map(|(i, (label, v))| {
                let on = v == current;
                let (v, pick) = (v.clone(), pick.clone());
                div()
                    .id((id, i))
                    .px_2p5()
                    .py_0p5()
                    .rounded_sm()
                    .cursor_pointer()
                    .text_color(rgb(if on { TEXT } else { DIM }))
                    .when(on, |d| d.bg(rgb(SELECT)))
                    .when(!on, |d| d.hover(|s| s.text_color(rgb(TEXT))))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        pick(this, v.clone(), window, cx);
                        this.save_settings();
                        cx.notify();
                    }))
                    .child(*label)
            }))
    }

    fn toggle(&self, id: &'static str, on: bool, flip: fn(&mut Prefs), cx: &mut Context<Self>) -> Stateful<Div> {
        switch(id, on).on_click(cx.listener(move |this, _, _, cx| {
            flip(&mut this.prefs);
            this.save_settings();
            cx.notify();
        }))
    }

    pub fn settings_view(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = &self.prefs;
        let general = panel("settings", "General")
            .child(row(
                "Theme",
                "",
                self.choice(
                    "theme",
                    &[("System", "system".to_string()), ("Light", "light".into()), ("Dark", "dark".into())],
                    &p.theme,
                    |this, v, window, cx| {
                        THEME.store(theme_code(&v), Relaxed);
                        this.prefs.theme = v;
                        apply_theme(window, cx);
                        window.refresh();
                    },
                    cx,
                ),
            ))
            .child(row(
                "Ask before deleting or redeploying",
                "A confirmation dialog; Enter confirms, Esc cancels",
                self.toggle("confirm", p.confirm, |p| p.confirm = !p.confirm, cx),
            ))
            .child(row(
                "Check for updates automatically",
                if DEV {
                    "Dev builds don't update themselves".to_string()
                } else {
                    format!("On launch and once a day · v{}", env!("CARGO_PKG_VERSION"))
                },
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when(!DEV, |d| {
                        d.child(
                            button("check-now", "download", "Check now", DIM)
                                .on_click(cx.listener(|this, _, _, cx| this.check_updates(true, cx))),
                        )
                    })
                    .child(self.toggle("auto-update", p.auto_update, |p| p.auto_update = !p.auto_update, cx)),
            ));

        let files = k8s::kubeconfig_files();
        let added = k8s::added_kubeconfig_files();
        let home = dirs::home_dir().map(|h| h.display().to_string()).unwrap_or_default();
        let clusters = panel("server", "Clusters")
            .child(div().text_xs().text_color(rgb(FAINT)).pb_1().child("KUBECONFIG FILES"))
            .children(files.into_iter().enumerate().map(|(i, f)| {
                let shown = f.display().to_string().replacen(&home, "~", 1);
                let removable = added.contains(&f);
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .py_1()
                    .child(icon("file-text", FAINT).size(px(13.)))
                    .child(div().flex_1().min_w_0().truncate().text_sm().font_family("Menlo").child(shown))
                    .child(if removable {
                        icon_button(("rm-kubeconfig", i), "x", DIM, "Remove from Masthead (the file stays)")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Err(e) = k8s::remove_kubeconfig_file(&f) {
                                    this.status = format!("Could not update the kubeconfig list: {e}");
                                }
                                this.reload_contexts(cx);
                                cx.notify();
                            }))
                            .into_any_element()
                    } else {
                        div().text_xs().text_color(rgb(FAINT)).child("default").into_any_element()
                    })
            }))
            .child(
                div().pt_1().pb_3().child(
                    button("add-kubeconfig-settings", "plus", "Add kubeconfig file…", DIM)
                        .on_click(cx.listener(|this, _, _, cx| this.add_kubeconfig(cx))),
                ),
            )
            .child(div().text_xs().text_color(rgb(FAINT)).pb_1().child("CONTEXTS"))
            .children(self.ctxs.iter().enumerate().map(|(i, c)| {
                let cp = p.contexts.get(&c.name).cloned().unwrap_or_default();
                let locked = self.protected.contains(&c.name);
                let (n1, n2, n3) = (c.name.clone(), c.name.clone(), c.name.clone());
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .id(("ctx-color", i))
                            .flex_none()
                            .size(px(14.))
                            .rounded_full()
                            .cursor_pointer()
                            .bg(rgb(self.ctx_color(c)))
                            .tooltip(tip("Click to change the color"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                let cp = this.ctx_prefs(&n1);
                                let at = PICKABLE.iter().position(|x| *x == cp.color).unwrap_or(0);
                                cp.color = PICKABLE[(at + 1) % PICKABLE.len()];
                                this.save_settings();
                                cx.notify();
                            })),
                    )
                    .child(div().w(px(240.)).truncate().text_sm().child(c.name.clone()))
                    .child(self.field(&format!("alias:{}", c.name), 180.))
                    .child(div().flex_1())
                    .child(
                        icon_button(
                            ("ctx-mute", i),
                            if cp.mute { "bell-off" } else { "bell" },
                            if cp.mute { YELLOW } else { DIM },
                            "Notifications",
                        )
                        .tooltip(tip(if cp.mute {
                            "Notifications off for this context"
                        } else {
                            "Notifications on"
                        }))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            let cp = this.ctx_prefs(&n2);
                            cp.mute = !cp.mute;
                            this.save_settings();
                            cx.notify();
                        })),
                    )
                    .child(
                        icon_button(("ctx-lock", i), "lock", if locked { YELLOW } else { DIM }, "Protected")
                            .tooltip(tip(if locked {
                                "Protected: changes are off. Click to allow changes"
                            } else {
                                "Protect: turn off deletes, scaling, redeploys and YAML saves"
                            }))
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_protected(n3.clone(), cx))),
                    )
            }));

        let notifications = panel("bell", "Notifications")
            .child(row(
                "Notify about new problems",
                "macOS notifications, also while Masthead is in the background",
                self.toggle("notify", p.notify, |p| p.notify = !p.notify, cx),
            ))
            .child(row(
                "Include warnings",
                "Off: errors only",
                self.toggle("notify-warnings", p.notify_warnings, |p| p.notify_warnings = !p.notify_warnings, cx),
            ))
            .child(row(
                "Sound",
                "",
                self.toggle("notify-sound", p.notify_sound, |p| p.notify_sound = !p.notify_sound, cx),
            ))
            .child(row(
                "Ignore namespaces",
                "Comma-separated, * wildcards. Mute whole contexts under Clusters.",
                self.field("ignore", 220.),
            ))
            .child(row(
                "Flux grace period (minutes)",
                "How long a Flux object may be not Ready before it counts as a problem",
                self.field("grace", 80.),
            ));

        let hidden = &p.hidden_kinds;
        let all = kinds();
        let sidebar = panel("layers", "Sidebar").child(
            div()
                .text_xs()
                .text_color(rgb(FAINT))
                .pb_2()
                .child("Untick kinds you never use to hide them from the sidebar. They stay in ⌘P search."),
        );
        let sidebar = GROUPS.iter().enumerate().fold(sidebar, |panel, (g, (label, _, start))| {
            let end = GROUPS.get(g + 1).map_or(k8s::BUILTIN_COUNT, |n| n.2);
            panel.child(
                div()
                    .flex()
                    .gap_3()
                    .py_1()
                    .child(
                        div()
                            .w(px(130.))
                            .flex_none()
                            .text_xs()
                            .text_color(rgb(FAINT))
                            .pt(px(3.))
                            .child(*label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .grid()
                            .grid_cols(4)
                            .gap_x_4()
                            .gap_y_1()
                            .children((*start..end).map(|i| {
                                let name = all[i].name;
                                let shown = !hidden.contains(name);
                                div()
                                    .id(("show-kind", i))
                                    .flex()
                                    .items_center()
                                    .gap_1p5()
                                    .cursor_pointer()
                                    .text_sm()
                                    .text_color(rgb(if shown { TEXT } else { FAINT }))
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if !this.prefs.hidden_kinds.remove(name) {
                                            this.prefs.hidden_kinds.insert(name.to_string());
                                        }
                                        this.save_settings();
                                        cx.notify();
                                    }))
                                    .child(checkbox(shown))
                                    .child(name)
                            })),
                    ),
            )
        });

        let tools = panel("square-terminal", "Tools")
            .child(row(
                "Terminal",
                "Where shells, debug containers and node shells open",
                self.choice(
                    "terminal",
                    &[
                        ("Auto", "auto".to_string()),
                        ("Ghostty", "ghostty".into()),
                        ("Terminal", "terminal".into()),
                        ("iTerm", "iterm".into()),
                    ],
                    &p.terminal,
                    |this, v, _, _| this.prefs.terminal = v,
                    cx,
                ),
            ))
            .child(row(
                "Debug image",
                "For debug containers and node shells (needs sh and nsenter)",
                self.field("image", 220.),
            ));

        let logs = panel("scroll-text", "Logs")
            .child(row(
                "Wrap long lines",
                "",
                self.toggle("log-wrap-default", p.log_wrap, |p| p.log_wrap = !p.log_wrap, cx),
            ))
            .child(row(
                "Show timestamps",
                "Default for new log views",
                self.toggle("log-ts-default", p.log_timestamps, |p| p.log_timestamps = !p.log_timestamps, cx),
            ))
            .child(row(
                "Load",
                "Default time range for new log views",
                self.choice(
                    "log-since-default",
                    &[("Recent", None), ("5m", Some(300)), ("1h", Some(3600)), ("24h", Some(86400))],
                    &p.log_since,
                    |this, v, _, _| this.prefs.log_since = v,
                    cx,
                ),
            ));

        let shortcuts = panel("command", "Keyboard shortcuts").children(SHORTCUTS.iter().map(|(keys, what)| {
            div()
                .flex()
                .gap_4()
                .py_0p5()
                .text_sm()
                .child(
                    div()
                        .w(px(200.))
                        .flex_none()
                        .font_family("Menlo")
                        .text_xs()
                        .text_color(rgb(DIM))
                        .child(*keys),
                )
                .child(div().text_color(rgb(TEXT)).child(*what))
        }));

        let data = panel("folder", "Data")
            .child(row(
                "Settings folder",
                k8s::app_dir().display().to_string().replacen(&home, "~", 1),
                button("reveal-settings", "folder", "Show in Finder", DIM).on_click(|_, _, _| {
                    let _ = std::process::Command::new("open").arg(k8s::app_dir()).spawn();
                }),
            ))
            .child(row(
                "Reset settings",
                "Everything on this page back to defaults. Protected contexts, kubeconfig files and sorting stay.",
                button(
                    "reset-settings",
                    "rotate-ccw",
                    if self.reset_armed { "Click again to reset" } else { "Reset…" },
                    if self.reset_armed { RED } else { DIM },
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    if !this.reset_armed {
                        this.reset_armed = true;
                        return cx.notify();
                    }
                    this.prefs = Prefs::default();
                    this.settings_inputs.clear();
                    this.reset_armed = false;
                    THEME.store(0, Relaxed);
                    apply_theme(window, cx);
                    this.save_settings();
                    this.status = "Settings reset".into();
                    cx.notify();
                })),
            ));

        div()
            .id("settings")
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.settings_scroll)
            .child(
                div()
                    .max_w(px(860.))
                    .px_6()
                    .py_5()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .child(general)
                    .child(clusters)
                    .child(notifications)
                    .child(sidebar)
                    .child(tools)
                    .child(logs)
                    .child(shortcuts)
                    .child(data),
            )
    }
}

#[test]
fn globs() {
    assert!(glob_match("dev-*", "dev-alex"));
    assert!(glob_match("*-test", "shop-test"));
    assert!(glob_match("kube-system", "kube-system"));
    assert!(!glob_match("kube-system", "kube-public"));
    assert!(glob_match("*", "anything"));
    assert!(glob_match("a*c*e", "abcde"));
    assert!(!glob_match("dev-*", "prod-dev-x"));
}
