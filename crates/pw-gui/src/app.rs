//! Root workspace: compact host sidebar + VS Code-style ports table + quick-add
//! bar, plus the host credentials panel.

use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::prelude::*;
use gpui::{
    actions, div, px, AnyElement, App, ClickEvent, Context, Entity, FocusHandle, KeyBinding,
    Render, SharedString, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::notification::Notification;
use gpui_component::switch::Switch;
use gpui_component::table::{Table, TableState};
use gpui_component::tag::Tag;
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    StyledExt as _, TitleBar, WindowExt as _,
};
use pw_core::{
    config::ThemePreference, AuthKind, CoreEvent, HostId, HostSource, HostView, Porty, SessionState,
};

use crate::ports_view::PortsDelegate;
use crate::theme;

actions!(
    porty,
    [
        QuickAdd,
        FocusSearch,
        SlashSearch,
        ToggleTheme,
        ConnectSelected,
        ImportHosts
    ]
);

/// Register global key bindings. Called once from `main`.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-n", QuickAdd, None),
        KeyBinding::new("ctrl-n", QuickAdd, None),
        KeyBinding::new("cmd-f", FocusSearch, None),
        KeyBinding::new("ctrl-f", FocusSearch, None),
        KeyBinding::new("/", SlashSearch, Some("porty")),
        KeyBinding::new("cmd-shift-l", ToggleTheme, None),
        KeyBinding::new("ctrl-shift-l", ToggleTheme, None),
        KeyBinding::new("cmd-enter", ConnectSelected, None),
        KeyBinding::new("ctrl-enter", ConnectSelected, None),
        KeyBinding::new("cmd-r", ImportHosts, None),
        KeyBinding::new("ctrl-r", ImportHosts, None),
    ]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ToastKind {
    Info,
    Success,
    Warn,
    Error,
}

struct Toast {
    kind: ToastKind,
    message: SharedString,
    created: Instant,
}

pub struct PortyApp {
    service: Arc<Porty>,
    hosts: Vec<HostView>,
    selected_host: Option<HostId>,
    table: Entity<TableState<PortsDelegate>>,
    search_input: Entity<InputState>,
    add_input: Entity<InputState>,
    alias_input: Entity<InputState>,
    address_input: Entity<InputState>,
    user_input: Entity<InputState>,
    port_input: Entity<InputState>,
    password_input: Entity<InputState>,
    key_input: Entity<InputState>,
    auth_kind: AuthKind,
    show_add_host: bool,
    show_host_settings: bool,
    theme_preference: ThemePreference,
    toast: Option<Toast>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<gpui::Subscription>,
}

impl PortyApp {
    pub fn new(service: Arc<Porty>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(PortsDelegate::new(service.clone()), window, cx)
                .col_resizable(true)
                .row_selectable(false)
        });

        let search_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter by port, process or host"));
        let add_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Forward a port — 3000, 8080:3000, 10.0.0.5:5432->15432")
        });
        let alias_input = cx.new(|cx| InputState::new(window, cx).placeholder("alias, e.g. prod"));
        let address_input = cx.new(|cx| InputState::new(window, cx).placeholder("host name or IP"));
        let user_input = cx.new(|cx| InputState::new(window, cx).placeholder("user (optional)"));
        let port_input = cx.new(|cx| InputState::new(window, cx).placeholder("port (default 22)"));
        let password_input = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder("password")
        });
        let key_input = cx.new(|cx| InputState::new(window, cx).placeholder("~/.ssh/id_ed25519"));

        let mut subscriptions = Vec::new();

        // Enter in the quick-add bar forwards the port.
        subscriptions.push(cx.subscribe_in(
            &add_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_quick_add(window, cx);
                }
            },
        ));

        // Live filtering.
        subscriptions.push(cx.subscribe_in(
            &search_input,
            window,
            |this, _input, event: &InputEvent, _window, cx| {
                if matches!(event, InputEvent::Change) {
                    this.apply_filters(cx);
                }
            },
        ));

        // Enter on the first add-host field submits the form.
        subscriptions.push(cx.subscribe_in(
            &address_input,
            window,
            |this, _input, event: &InputEvent, window, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    this.submit_add_host(window, cx);
                }
            },
        ));

        let theme_preference = service.theme_preference();
        let focus_handle = cx.focus_handle();

        let mut this = Self {
            service,
            hosts: Vec::new(),
            selected_host: None,
            table,
            search_input,
            add_input,
            alias_input,
            address_input,
            user_input,
            port_input,
            password_input,
            key_input,
            auth_kind: AuthKind::Agent,
            show_add_host: false,
            show_host_settings: false,
            theme_preference,
            toast: None,
            focus_handle,
            _subscriptions: subscriptions,
        };
        this.refresh_hosts(cx);
        this.focus_handle.focus(window);

        // Core event pump: the service runs on Tokio, the UI on GPUI's executor.
        // `broadcast::Receiver::recv` is executor agnostic, so awaiting it from
        // a GPUI task is safe and keeps the UI at full refresh rate.
        let service = this.service.clone();
        cx.spawn(async move |weak, cx| {
            let mut events = service.subscribe();
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if weak
                            .update(cx, |this, cx| this.on_core_event(event, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        if weak
                            .update(cx, |this, cx| {
                                this.refresh_hosts(cx);
                                this.refresh_table(cx);
                                this.set_toast(
                                    ToastKind::Warn,
                                    format!("UI fell behind, skipped {skipped} events"),
                                );
                                cx.notify();
                            })
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .detach();

        // One-second heartbeat: refreshes RTT, live traffic counters and
        // expires transient banners without any polling from the render path.
        cx.spawn(async move |weak, cx| loop {
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
            if weak.update(cx, |this, cx| this.tick(cx)).is_err() {
                break;
            }
        })
        .detach();

        this
    }

    fn tick(&mut self, cx: &mut Context<Self>) {
        self.refresh_hosts(cx);
        self.refresh_table(cx);
        if let Some(toast) = &self.toast {
            if toast.created.elapsed() > Duration::from_secs(6) {
                self.toast = None;
            }
        }
        cx.notify();
    }

    fn set_toast(&mut self, kind: ToastKind, message: impl Into<SharedString>) {
        self.toast = Some(Toast {
            kind,
            message: message.into(),
            created: Instant::now(),
        });
    }

    fn on_core_event(&mut self, event: CoreEvent, cx: &mut Context<Self>) {
        match event {
            CoreEvent::HostStateChanged { state, detail, .. } => {
                self.refresh_hosts(cx);
                self.refresh_table(cx);
                if let Some(detail) = detail {
                    if matches!(
                        state,
                        SessionState::Reconnecting { .. } | SessionState::Error { .. }
                    ) {
                        self.set_toast(ToastKind::Warn, detail);
                    }
                }
            }
            CoreEvent::HostMetrics { .. } => self.refresh_hosts(cx),
            CoreEvent::HostsChanged => {
                self.refresh_hosts(cx);
                self.refresh_table(cx);
            }
            CoreEvent::RuleUpdated(_) | CoreEvent::RuleRemoved { .. } => self.refresh_table(cx),
            CoreEvent::RemotePortDiscovered { host_id, port } => {
                let (host, auto_forward) = self
                    .hosts
                    .iter()
                    .find(|host| host.config.id == host_id)
                    .map(|host| (host.config.alias.clone(), host.config.auto_forward))
                    .unwrap_or((host_id, false));
                // When auto-forward is on the rule appears in the table
                // immediately; a toast for every port would be noise.
                if !auto_forward {
                    let process = port
                        .process_name
                        .as_deref()
                        .map(|name| format!(" ({name})"))
                        .unwrap_or_default();
                    self.set_toast(
                        ToastKind::Info,
                        format!("New port {}{process} is listening on {host}", port.port),
                    );
                }
            }
            CoreEvent::RemotePortClosed { .. } => {}
            CoreEvent::Log { message, .. } => self.set_toast(ToastKind::Warn, message),
        }
        cx.notify();
    }

    fn refresh_hosts(&mut self, cx: &mut Context<Self>) {
        self.hosts = self.service.hosts();
        if let Some(selected) = &self.selected_host {
            if !self.hosts.iter().any(|host| &host.config.id == selected) {
                self.selected_host = None;
                self.apply_filters(cx);
            }
        }
    }

    fn refresh_table(&mut self, cx: &mut Context<Self>) {
        self.table.update(cx, |state, cx| {
            state.delegate_mut().reload();
            state.refresh(cx);
        });
    }

    fn apply_filters(&mut self, cx: &mut Context<Self>) {
        let host_filter = self.selected_host.clone();
        let query = self.search_input.read(cx).value().to_string();
        self.table.update(cx, |state, cx| {
            state.delegate_mut().set_host_filter(host_filter);
            state.delegate_mut().set_query(query);
            state.refresh(cx);
        });
    }

    fn select_host(
        &mut self,
        host_id: Option<HostId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.selected_host != host_id;
        self.selected_host = host_id.clone();

        if let Some(id) = &host_id {
            if let Some(host) = self.hosts.iter().find(|host| &host.config.id == id) {
                self.auth_kind = host.config.auth;
                if changed {
                    let key_path = host
                        .config
                        .identity_file
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_default();
                    self.password_input
                        .update(cx, |state, cx| state.set_value("", window, cx));
                    self.key_input
                        .update(cx, |state, cx| state.set_value(key_path, window, cx));
                }
            }
        }

        self.apply_filters(cx);
        cx.notify();
    }

    fn selected_host_view(&self) -> Option<&HostView> {
        let id = self.selected_host.as_ref()?;
        self.hosts.iter().find(|host| &host.config.id == id)
    }

    fn focus_quick_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.add_input
            .update(cx, |state, cx| state.focus(window, cx));
    }

    fn toggle_connection(&mut self, host_id: HostId, window: &mut Window, cx: &mut Context<Self>) {
        let state = self.service.session_state(&host_id);
        let result = match state {
            SessionState::Disconnected | SessionState::Error { .. } => {
                self.service.connect(&host_id)
            }
            _ => {
                self.service.disconnect(&host_id);
                Ok(())
            }
        };
        match result {
            Ok(()) => {
                if let Some(host) = self.hosts.iter().find(|host| host.config.id == host_id) {
                    let message = if host.state.is_connected() {
                        format!("Disconnecting from {}", host.config.alias)
                    } else {
                        format!("Connecting to {}", host.config.alias)
                    };
                    self.set_toast(ToastKind::Info, message);
                }
            }
            Err(err) => {
                self.set_toast(ToastKind::Error, format!("{err:#}"));
                window.push_notification(Notification::error(format!("{err:#}")), cx);
            }
        }
        self.refresh_hosts(cx);
        cx.notify();
    }

    fn submit_quick_add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let spec = self.add_input.read(cx).value().to_string();
        if spec.trim().is_empty() {
            return;
        }

        let host_id = self.selected_host.clone().or_else(|| {
            if self.hosts.len() == 1 {
                Some(self.hosts[0].config.id.clone())
            } else {
                None
            }
        });
        let Some(host_id) = host_id else {
            self.set_toast(ToastKind::Warn, "Select a host in the sidebar first.");
            cx.notify();
            return;
        };

        match self.service.add_rule_from_spec(&host_id, &spec, None) {
            Ok(_) => {
                self.add_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.set_toast(ToastKind::Success, format!("Forwarding {spec}"));
                self.refresh_table(cx);
            }
            Err(err) => {
                self.set_toast(ToastKind::Error, format!("{err:#}"));
                window.push_notification(Notification::error(format!("{err:#}")), cx);
            }
        }
        cx.notify();
    }

    fn submit_add_host(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let alias = self.alias_input.read(cx).value().to_string();
        let address = self.address_input.read(cx).value().to_string();
        let user = self.user_input.read(cx).value().to_string();
        let port_text = self.port_input.read(cx).value().to_string();
        let port: u16 = if port_text.trim().is_empty() {
            22
        } else {
            match port_text.trim().parse() {
                Ok(port) => port,
                Err(_) => {
                    self.set_toast(
                        ToastKind::Error,
                        "Port must be a number between 1 and 65535",
                    );
                    cx.notify();
                    return;
                }
            }
        };

        let user = if user.trim().is_empty() {
            None
        } else {
            Some(user.trim().to_string())
        };

        match self
            .service
            .add_manual_host(alias.trim(), address.trim(), port, user)
        {
            Ok(id) => {
                for input in [
                    &self.alias_input,
                    &self.address_input,
                    &self.user_input,
                    &self.port_input,
                ] {
                    input.update(cx, |state, cx| state.set_value("", window, cx));
                }
                self.show_add_host = false;
                self.set_toast(ToastKind::Success, format!("Added host {id}"));
                self.select_host(Some(id), window, cx);
                self.refresh_hosts(cx);
            }
            Err(err) => {
                self.set_toast(ToastKind::Error, format!("{err:#}"));
                window.push_notification(Notification::error(format!("{err:#}")), cx);
            }
        }
        cx.notify();
    }

    fn import_hosts(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.service.import_ssh_config() {
            Ok(count) => {
                self.refresh_hosts(cx);
                self.set_toast(
                    ToastKind::Success,
                    format!("Imported ~/.ssh/config — {count} hosts known"),
                );
            }
            Err(err) => {
                self.set_toast(ToastKind::Error, format!("{err:#}"));
                window.push_notification(Notification::error(format!("{err:#}")), cx);
            }
        }
        cx.notify();
    }

    fn toggle_theme(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dark = cx.theme().mode.is_dark();
        self.theme_preference = if dark {
            ThemePreference::Light
        } else {
            ThemePreference::Dark
        };
        theme::apply(self.theme_preference, window, cx);
        self.service.set_theme_preference(self.theme_preference);
        cx.notify();
    }

    fn save_password_and_connect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(host_id) = self.selected_host.clone() else {
            return;
        };
        let password = self.password_input.read(cx).value().to_string();
        if password.is_empty() {
            self.set_toast(ToastKind::Warn, "Enter a password first");
            cx.notify();
            return;
        }
        let result = self
            .service
            .set_password(&host_id, Some(password))
            .and_then(|_| self.service.set_auth(&host_id, AuthKind::Password, None))
            .and_then(|_| {
                // A session may be connecting or reconnecting; reconnect in
                // that case so the new credentials are actually used.
                if matches!(
                    self.service.session_state(&host_id),
                    SessionState::Disconnected
                ) {
                    self.service.connect(&host_id)
                } else {
                    self.service.reconnect(&host_id)
                }
            });
        match result {
            Ok(()) => {
                self.auth_kind = AuthKind::Password;
                self.password_input
                    .update(cx, |state, cx| state.set_value("", window, cx));
                self.set_toast(ToastKind::Info, "Connecting with password…");
            }
            Err(err) => self.set_toast(ToastKind::Error, format!("{err:#}")),
        }
        cx.notify();
    }

    fn save_key_and_connect(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(host_id) = self.selected_host.clone() else {
            return;
        };
        let key = self.key_input.read(cx).value().to_string();
        if key.trim().is_empty() {
            self.set_toast(ToastKind::Warn, "Enter the path to a private key");
            cx.notify();
            return;
        }
        let path = std::path::PathBuf::from(shellexpand_tilde(key.trim()));
        let result = self
            .service
            .set_auth(&host_id, AuthKind::Key, Some(path))
            .and_then(|_| self.service.reconnect(&host_id));
        match result {
            Ok(()) => {
                self.auth_kind = AuthKind::Key;
                self.set_toast(ToastKind::Info, "Connecting with private key…");
            }
            Err(err) => self.set_toast(ToastKind::Error, format!("{err:#}")),
        }
        cx.notify();
    }

    // ------------------------------------------------------------ rendering

    fn render_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rule_count = self.service.rules().len();
        let dark = cx.theme().mode.is_dark();
        let theme_icon = if dark { IconName::Sun } else { IconName::Moon };

        TitleBar::new().child(
            h_flex()
                .w_full()
                .h_full()
                .px_3()
                .items_center()
                .justify_between()
                .child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(
                            Icon::new(IconName::Globe)
                                .size_4()
                                .text_color(cx.theme().primary),
                        )
                        .child(div().text_sm().font_semibold().child("Porty"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!(
                                    "{} hosts · {} forwarded",
                                    self.hosts.len(),
                                    rule_count
                                )),
                        ),
                )
                .child(
                    h_flex().gap_1().items_center().child(
                        Button::new("title-theme")
                            .icon(Icon::new(theme_icon))
                            .ghost()
                            .xsmall()
                            .tooltip("Toggle light / dark")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.toggle_theme(window, cx)
                            })),
                    ),
                ),
        )
    }

    fn render_all_hosts_row(&self, rule_count: usize, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected_host.is_none();
        let hover = cx.theme().sidebar_accent.opacity(0.55);
        h_flex()
            .id("all-hosts")
            .px_3()
            .py_2()
            .gap_2()
            .items_center()
            .justify_between()
            .cursor_pointer()
            .when(selected, |this| this.bg(cx.theme().sidebar_accent))
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.select_host(None, window, cx);
            }))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Icon::new(IconName::Globe)
                            .size_3p5()
                            .text_color(cx.theme().muted_foreground),
                    )
                    .child(div().text_sm().font_medium().child("All hosts")),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("{rule_count} ports")),
            )
            .into_any_element()
    }

    fn render_sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let host_rows: Vec<AnyElement> = self
            .hosts
            .iter()
            .enumerate()
            .map(|(index, host)| self.host_row(host, index, cx))
            .collect();
        let host_count = self.hosts.len();
        let rule_count = self.service.rules().len();
        let show_add = self.show_add_host;

        let mut sidebar = v_flex()
            .w(px(theme::SIDEBAR_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("HOSTS · {host_count}")),
                    )
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Button::new("import-hosts")
                                    .icon(Icon::empty().path("icons/refresh-cw.svg"))
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Re-import ~/.ssh/config")
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.import_hosts(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("toggle-add-host")
                                    .icon(Icon::new(IconName::Plus))
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Add a host manually")
                                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                        this.show_add_host = !this.show_add_host;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .id("host-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .py_1()
                    .child(self.render_all_hosts_row(rule_count, cx))
                    .children(host_rows),
            );

        if show_add {
            sidebar = sidebar.child(self.render_add_host_form(cx));
        }

        sidebar
            .child(
                h_flex()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(cx.theme().sidebar_border)
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{rule_count} forwarded ports")),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("v0.1.0"),
                    ),
            )
            .into_any_element()
    }

    fn host_row(&self, host: &HostView, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let host_id = host.config.id.clone();
        let selected = self.selected_host.as_deref() == Some(host_id.as_str());
        let color = theme::session_color(&host.state, cx);
        let hover = cx.theme().sidebar_accent.opacity(0.55);
        let rtt = host
            .rtt_ms
            .map(|rtt| format!("{rtt} ms"))
            .unwrap_or_default();
        let connected = host.state.is_connected();
        let busy = matches!(
            host.state,
            SessionState::Connecting
                | SessionState::Authenticating
                | SessionState::Reconnecting { .. }
        );

        let mut row = v_flex()
            .id(("host-row", index))
            .px_3()
            .py_2()
            .gap_1()
            .cursor_pointer()
            .when(selected, |this| this.bg(cx.theme().sidebar_accent))
            .hover(move |style| style.bg(hover))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.select_host(Some(host_id.clone()), window, cx);
            }))
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .min_w_0()
                            .child(theme::status_dot(color))
                            .child(
                                div()
                                    .text_sm()
                                    .font_medium()
                                    .truncate()
                                    .child(host.config.alias.clone()),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(rtt),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .truncate()
                            .child(host.config.display_target()),
                    )
                    .child(div().text_xs().text_color(color).child(host.state.label())),
            );

        if selected {
            let connect_label = if connected || busy {
                "Disconnect"
            } else {
                "Connect"
            };
            let host_id_for_button = host.config.id.clone();
            let button = Button::new(("connect", index))
                .xsmall()
                .label(connect_label);
            let button = if connected || busy {
                button.outline()
            } else {
                button.primary()
            };

            row = row
                .child(
                    v_flex()
                        .gap_1()
                        .pt_1()
                        .border_t_1()
                        .border_color(cx.theme().sidebar_border)
                        .child(self.toggle_row(
                            host,
                            index,
                            "Auto-discover ports",
                            host.config.auto_discover,
                            HostToggle::Discover,
                            cx,
                        ))
                        .child(self.toggle_row(
                            host,
                            index,
                            "Bind 0.0.0.0",
                            host.config.bind_all,
                            HostToggle::BindAll,
                            cx,
                        ))
                        .child(self.toggle_row(
                            host,
                            index,
                            "Auto-forward new",
                            host.config.auto_forward,
                            HostToggle::AutoForward,
                            cx,
                        )),
                )
                .child(
                    h_flex()
                        .gap_1()
                        .pt_1()
                        .child(button.on_click(cx.listener(
                            move |this, _: &ClickEvent, window, cx| {
                                this.toggle_connection(host_id_for_button.clone(), window, cx);
                            },
                        )))
                        .child(
                            Button::new(("host-settings", index))
                                .xsmall()
                                .label("Credentials")
                                .ghost()
                                .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                    this.show_host_settings = !this.show_host_settings;
                                    cx.notify();
                                })),
                        ),
                );
        }

        row.into_any_element()
    }

    fn toggle_row(
        &self,
        host: &HostView,
        index: usize,
        label: &'static str,
        checked: bool,
        which: HostToggle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let service = self.service.clone();
        let host_id = host.config.id.clone();
        let id = match which {
            HostToggle::Discover => ("discover", index),
            HostToggle::BindAll => ("bind-all", index),
            HostToggle::AutoForward => ("auto-forward", index),
        };
        h_flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label),
            )
            .child(
                Switch::new(id)
                    .checked(checked)
                    .xsmall()
                    .on_click(move |next, _, _| {
                        let next = *next;
                        let result = match which {
                            HostToggle::Discover => {
                                service.set_host_preferences(&host_id, Some(next), None, None)
                            }
                            HostToggle::AutoForward => {
                                service.set_host_preferences(&host_id, None, Some(next), None)
                            }
                            HostToggle::BindAll => {
                                service.set_host_preferences(&host_id, None, None, Some(next))
                            }
                        };
                        if let Err(err) = result {
                            tracing::warn!("could not update host preference: {err:#}");
                        }
                    }),
            )
            .into_any_element()
    }

    fn render_add_host_form(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .px_3()
            .py_2()
            .gap_2()
            .border_t_1()
            .border_color(cx.theme().sidebar_border)
            .child(div().text_xs().font_semibold().child("ADD SSH HOST"))
            .child(Input::new(&self.alias_input).cleanable(true))
            .child(Input::new(&self.address_input).cleanable(true))
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.user_input).cleanable(true)),
                    )
                    .child(
                        div()
                            .w(px(96.))
                            .child(Input::new(&self.port_input).cleanable(true)),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel-add-host")
                            .xsmall()
                            .label("Cancel")
                            .outline()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.show_add_host = false;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("save-add-host")
                            .xsmall()
                            .label("Add host")
                            .primary()
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.submit_add_host(window, cx)
                            })),
                    ),
            )
    }

    fn render_host_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let Some(host) = self.selected_host_view().cloned() else {
            return div().into_any_element();
        };
        let host_id = host.config.id.clone();
        let identity_files = if host.config.identity_files.is_empty() {
            "none found — will use the SSH agent".to_string()
        } else {
            host.config
                .identity_files
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let is_manual = host.config.source == HostSource::Manual;

        let auth_buttons: Vec<AnyElement> = [AuthKind::Agent, AuthKind::Key, AuthKind::Password]
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                let selected = self.auth_kind == kind;
                let service = self.service.clone();
                let host_id = host_id.clone();
                let mut button = Button::new(("auth-kind", index))
                    .xsmall()
                    .label(kind.label());
                button = if selected {
                    button.primary()
                } else {
                    button.ghost()
                };
                button
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.auth_kind = kind;
                        if let Err(err) = service.set_auth(&host_id, kind, None) {
                            this.set_toast(ToastKind::Error, format!("{err:#}"));
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            })
            .collect();

        let host_id_remove = host_id.clone();
        let host_id_reconnect = host_id.clone();

        v_flex()
            .px_4()
            .py_3()
            .gap_2()
            .bg(cx.theme().secondary.opacity(0.35))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child("AUTHENTICATION"),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("Target: {}", host.config.display_target())),
                    )
                    .children(auth_buttons),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.password_input).cleanable(true)),
                    )
                    .child(
                        Button::new("save-password")
                            .xsmall()
                            .label("Use password")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.save_password_and_connect(window, cx)
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.key_input).cleanable(true)),
                    )
                    .child(Button::new("save-key").xsmall().label("Use key").on_click(
                        cx.listener(|this, _: &ClickEvent, window, cx| {
                            this.save_key_and_connect(window, cx)
                        }),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Identity files: {identity_files}")),
            )
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(
                        Button::new("host-reconnect")
                            .xsmall()
                            .label("Reconnect")
                            .outline()
                            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                                if let Err(err) = this.service.reconnect(&host_id_reconnect) {
                                    this.set_toast(ToastKind::Error, format!("{err:#}"));
                                    window.push_notification(
                                        Notification::error(format!("{err:#}")),
                                        cx,
                                    );
                                }
                                this.refresh_hosts(cx);
                                cx.notify();
                            })),
                    )
                    .when(is_manual, |this| {
                        this.child(
                            Button::new("host-remove")
                                .xsmall()
                                .label("Remove host")
                                .danger()
                                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                                    if let Err(err) = this.service.remove_host(&host_id_remove) {
                                        this.set_toast(ToastKind::Error, format!("{err:#}"));
                                    }
                                    this.selected_host = None;
                                    this.refresh_hosts(cx);
                                    this.apply_filters(cx);
                                    this.show_host_settings = false;
                                    cx.notify();
                                })),
                        )
                    }),
            )
            .into_any_element()
    }

    fn render_header(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let host = self.selected_host_view().cloned();
        let title = host
            .as_ref()
            .map(|host| host.config.alias.clone())
            .unwrap_or_else(|| "All hosts".to_string());
        let subtitle = host
            .as_ref()
            .map(|host| host.config.display_target())
            .unwrap_or_else(|| {
                format!(
                    "{} hosts · {} forwarded ports",
                    self.hosts.len(),
                    self.service.rules().len()
                )
            });
        let state = host.as_ref().map(|host| host.state.clone());
        let dark = cx.theme().mode.is_dark();
        let theme_icon = if dark { IconName::Sun } else { IconName::Moon };

        let status_tag = state.map(|state| {
            let color = theme::session_color(&state, cx);
            Tag::custom(color.opacity(0.14), color, color.opacity(0.32))
                .xsmall()
                .child(state.label())
        });

        h_flex()
            .px_4()
            .py_2()
            .gap_4()
            .items_center()
            .justify_between()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                v_flex()
                    .gap_0p5()
                    .min_w_0()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_base().font_semibold().child(title))
                            .children(status_tag),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(subtitle),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .w(px(236.))
                            .child(Input::new(&self.search_input).cleanable(true)),
                    )
                    .child(
                        Button::new("header-settings")
                            .icon(Icon::new(IconName::Settings2))
                            .ghost()
                            .xsmall()
                            .tooltip("Host credentials")
                            .disabled(host.is_none())
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.show_host_settings = !this.show_host_settings;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("header-theme")
                            .icon(Icon::new(theme_icon))
                            .ghost()
                            .xsmall()
                            .tooltip("Toggle light / dark")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.toggle_theme(window, cx)
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_quick_add(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let host_hint = self
            .selected_host
            .as_ref()
            .map(|host| format!("for {host}"))
            .unwrap_or_else(|| "select a host".to_string());
        h_flex()
            .px_4()
            .py_3()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .child(
                Icon::new(IconName::Plus)
                    .size_4()
                    .text_color(cx.theme().primary),
            )
            .child(
                div()
                    .flex_1()
                    .child(Input::new(&self.add_input).cleanable(true)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(host_hint),
            )
            .child(
                Button::new("quick-forward")
                    .primary()
                    .label("Forward")
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.submit_quick_add(window, cx)
                    })),
            )
            .into_any_element()
    }

    fn render_toast(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let toast = self.toast.as_ref()?;
        let (color, icon) = match toast.kind {
            ToastKind::Info => (cx.theme().info, IconName::Info),
            ToastKind::Success => (cx.theme().success, IconName::CircleCheck),
            ToastKind::Warn => (cx.theme().warning, IconName::TriangleAlert),
            ToastKind::Error => (cx.theme().danger, IconName::CircleX),
        };
        Some(
            h_flex()
                .px_4()
                .py_2()
                .gap_2()
                .items_center()
                .bg(color.opacity(0.1))
                .border_b_1()
                .border_color(color.opacity(0.28))
                .child(Icon::new(icon).size_4().text_color(color))
                .child(div().flex_1().text_sm().child(toast.message.clone()))
                .into_any_element(),
        )
    }

    fn render_welcome(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .flex_1()
            .size_full()
            .items_center()
            .justify_center()
            .gap_3()
            .child(
                Icon::new(IconName::Globe)
                    .size_10()
                    .text_color(cx.theme().primary),
            )
            .child(div().text_lg().font_semibold().child("Welcome to Porty"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child("Import your ~/.ssh/config, or add a host manually."),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(
                        Button::new("welcome-import")
                            .primary()
                            .label("Import ~/.ssh/config")
                            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                this.import_hosts(window, cx)
                            })),
                    )
                    .child(
                        Button::new("welcome-add")
                            .outline()
                            .label("Add a host")
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.show_add_host = true;
                                cx.notify();
                            })),
                    ),
            )
            .into_any_element()
    }
}

#[derive(Clone, Copy)]
enum HostToggle {
    Discover,
    AutoForward,
    BindAll,
}

/// Expand `~/` in a user-entered key path without pulling in a dependency.
fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
            return std::path::Path::new(&home).join(rest).display().to_string();
        }
    }
    path.to_string()
}

impl Render for PortyApp {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let toast = self.render_toast(cx);
        let welcome = self.hosts.is_empty();
        let host_settings = if self.show_host_settings && self.selected_host.is_some() {
            Some(self.render_host_settings(cx))
        } else {
            None
        };

        let main_content = if welcome {
            self.render_welcome(cx)
        } else {
            div()
                .flex_1()
                .min_h_0()
                .px_3()
                .pt_3()
                .pb_2()
                // Large rows give the two-line cells (mapping, process,
                // activity) room to breathe; the table controls row height.
                .child(Table::new(&self.table).stripe(false).large())
                .into_any_element()
        };

        v_flex()
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .key_context("porty")
            .track_focus(&self.focus_handle)
            .on_action(
                cx.listener(|this, _: &QuickAdd, window, cx| this.focus_quick_add(window, cx)),
            )
            .on_action(cx.listener(|this, _: &FocusSearch, window, cx| {
                this.search_input
                    .update(cx, |state, cx| state.focus(window, cx));
            }))
            .on_action(cx.listener(|this, _: &SlashSearch, window, cx| {
                // `/` is a Vim-style filter shortcut, but must never steal
                // the keystroke while the user is typing in a field.
                if window.has_focused_input(cx) {
                    return;
                }
                this.search_input
                    .update(cx, |state, cx| state.focus(window, cx));
            }))
            .on_action(
                cx.listener(|this, _: &ToggleTheme, window, cx| this.toggle_theme(window, cx)),
            )
            .on_action(cx.listener(|this, _: &ConnectSelected, window, cx| {
                if let Some(host_id) = this.selected_host.clone() {
                    this.toggle_connection(host_id, window, cx);
                }
            }))
            .on_action(
                cx.listener(|this, _: &ImportHosts, window, cx| this.import_hosts(window, cx)),
            )
            .child(self.render_title_bar(cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_sidebar(cx))
                    .child(
                        v_flex()
                            .flex_1()
                            .h_full()
                            .min_w_0()
                            .child(self.render_header(cx))
                            .children(toast)
                            .children(host_settings)
                            .child(main_content)
                            .child(self.render_quick_add(cx)),
                    ),
            )
    }
}
