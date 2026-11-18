//! The VS Code-style "Ports" table.
//!
//! Rendering is delegated to gpui-component's virtualized [`Table`], so the
//! panel stays smooth with hundreds of rules while every action works against
//! the headless [`Porty`] service.

use std::sync::Arc;

use gpui::prelude::*;
use gpui::{div, px, AnyElement, App, ClipboardItem, Context, Div, Stateful, Window};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::notification::Notification;
use gpui_component::switch::Switch;
use gpui_component::table::{Column, TableDelegate, TableState};
use gpui_component::tag::Tag;
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _,
    StyledExt as _, WindowExt as _,
};
use pw_core::{HostId, Porty, RuleId, RuleView};

use crate::theme;

pub struct PortsDelegate {
    service: Arc<Porty>,
    columns: Vec<Column>,
    rows: Vec<RuleRow>,
    host_filter: Option<HostId>,
    query: String,
}

#[derive(Clone)]
pub struct RuleRow {
    pub rule: RuleView,
    pub host_alias: String,
    pub host_connected: bool,
}

impl PortsDelegate {
    pub fn new(service: Arc<Porty>) -> Self {
        let columns = vec![
            Column::new("state", "STATE")
                .width(px(132.))
                .resizable(false),
            Column::new("mapping", "PORT MAPPING").width(px(210.)),
            Column::new("process", "PROCESS / LABEL").width(px(180.)),
            Column::new("visibility", "VISIBILITY")
                .width(px(96.))
                .resizable(false),
            Column::new("activity", "ACTIVITY").width(px(156.)),
            Column::new("actions", "")
                .width(px(148.))
                .resizable(false)
                .movable(false),
        ];

        let mut delegate = Self {
            service,
            columns,
            rows: Vec::new(),
            host_filter: None,
            query: String::new(),
        };
        delegate.reload();
        delegate
    }

    /// Refresh rows from the service snapshot.
    pub fn reload(&mut self) {
        let rules = self.service.rules();
        let hosts = self.service.hosts();
        let query = self.query.to_lowercase();

        let mut rows = Vec::new();
        for rule in rules {
            if let Some(filter) = &self.host_filter {
                if &rule.host_id != filter {
                    continue;
                }
            }

            let host = hosts.iter().find(|host| host.config.id == rule.host_id);
            let host_alias = host
                .map(|host| host.config.alias.clone())
                .unwrap_or_else(|| rule.host_id.clone());
            let host_connected = host.map(|host| host.state.is_connected()).unwrap_or(false);

            if !query.is_empty() {
                let haystack = format!(
                    "{} {} {} {} {} {}",
                    rule.display_name(),
                    host_alias,
                    rule.remote_host,
                    rule.remote_port,
                    rule.requested_local_port,
                    rule.local_address().unwrap_or_default(),
                )
                .to_lowercase();
                if !haystack.contains(&query) {
                    continue;
                }
            }

            rows.push(RuleRow {
                rule,
                host_alias,
                host_connected,
            });
        }

        // Active mappings first, then keep a stable port order.
        rows.sort_by_key(|row| {
            (
                !row.rule.state.is_active(),
                row.rule.remote_port,
                row.rule.id,
            )
        });
        self.rows = rows;
    }

    pub fn set_host_filter(&mut self, host_id: Option<HostId>) {
        self.host_filter = host_id;
        self.reload();
    }

    pub fn set_query(&mut self, query: String) {
        self.query = query;
        self.reload();
    }

    fn row(&self, index: usize) -> Option<RuleRow> {
        self.rows.get(index).cloned()
    }

    fn state_cell(&self, row: &RuleRow, cx: &mut Context<TableState<Self>>) -> AnyElement {
        let rule = &row.rule;
        let color = theme::rule_color(&rule.state, cx);
        let label_color = if matches!(rule.state, pw_core::RuleState::Error { .. }) {
            cx.theme().danger
        } else {
            cx.theme().foreground
        };

        let tag = if rule.auto_discovered {
            Some(Tag::info().xsmall().child("AUTO").into_any_element())
        } else if rule.bumped {
            Some(Tag::warning().xsmall().child("BUMPED").into_any_element())
        } else {
            None
        };

        h_flex()
            .gap_2()
            .items_center()
            .px_3()
            .child(theme::dot(color))
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .text_color(label_color)
                    .child(rule.state.label().to_string()),
            )
            .children(tag)
            .into_any_element()
    }

    fn mapping_cell(&self, row: &RuleRow, cx: &mut Context<TableState<Self>>) -> AnyElement {
        let rule = &row.rule;
        let mono = cx.theme().mono_font_family.clone();
        let remote = if rule.remote_host == "127.0.0.1" {
            format!(":{}", rule.remote_port)
        } else {
            format!("{}:{}", rule.remote_host, rule.remote_port)
        };
        let local = rule
            .local_address()
            .unwrap_or_else(|| "not bound".to_string());
        let local_color = if rule.bumped {
            cx.theme().warning
        } else if rule.local_port.is_some() {
            cx.theme().foreground
        } else {
            cx.theme().muted_foreground
        };

        h_flex()
            .gap_1p5()
            .items_center()
            .px_3()
            .child(
                div()
                    .text_sm()
                    .font_family(mono.clone())
                    .truncate()
                    .child(remote),
            )
            .child(
                Icon::new(IconName::ArrowRight)
                    .size_3()
                    .flex_shrink_0()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .font_family(mono)
                    .truncate()
                    .text_color(local_color)
                    .child(local),
            )
            .into_any_element()
    }

    fn process_cell(&self, row: &RuleRow, cx: &mut Context<TableState<Self>>) -> AnyElement {
        let (description, description_color) = match &row.rule.state {
            pw_core::RuleState::Error { message } => {
                (theme::truncate(message, 40), cx.theme().danger)
            }
            _ => (row.rule.display_name(), cx.theme().foreground),
        };

        h_flex()
            .gap_1p5()
            .items_center()
            .px_3()
            .child(
                Icon::new(IconName::SquareTerminal)
                    .size_3p5()
                    .flex_shrink_0()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .truncate()
                    .text_color(description_color)
                    .child(description),
            )
            .child(
                div()
                    .text_xs()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .child(if row.host_connected {
                        format!("· {}", row.host_alias)
                    } else {
                        format!("· {} (offline)", row.host_alias)
                    }),
            )
            .into_any_element()
    }

    fn visibility_cell(&self, row: &RuleRow, cx: &mut Context<TableState<Self>>) -> AnyElement {
        let rule = &row.rule;
        let service = self.service.clone();
        let table = cx.entity();
        let rule_id = rule.id;
        let bind_all = rule.bind_all;

        h_flex()
            .gap_2()
            .items_center()
            .px_3()
            .child(
                Switch::new(("visibility", rule_id as usize))
                    .checked(bind_all)
                    .xsmall()
                    .tooltip("Expose on 0.0.0.0 for LAN / Tailscale access")
                    .on_click(move |next, window, cx| {
                        if let Err(err) = service.set_rule_bind_all(rule_id, *next) {
                            window.push_notification(Notification::error(format!("{err:#}")), cx);
                        }
                        table.update(cx, |state, cx| {
                            state.delegate_mut().reload();
                            state.refresh(cx);
                        });
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if bind_all { "LAN" } else { "Local" }),
            )
            .into_any_element()
    }

    fn activity_cell(&self, row: &RuleRow, cx: &mut Context<TableState<Self>>) -> AnyElement {
        let rule = &row.rule;
        let live = rule.active_connections > 0;
        h_flex()
            .gap_1p5()
            .items_center()
            .px_3()
            .child(theme::dot(if live {
                cx.theme().success
            } else {
                cx.theme().muted_foreground.opacity(0.5)
            }))
            .child(div().text_sm().truncate().child(theme::dense_activity(
                rule.active_connections,
                rule.total_bytes(),
            )))
            .into_any_element()
    }

    fn actions_cell(&self, row: &RuleRow, cx: &mut Context<TableState<Self>>) -> AnyElement {
        let rule = &row.rule;
        let rule_id: RuleId = rule.id;
        let service = self.service.clone();
        let table = cx.entity();
        let running = rule.state.is_active();
        let address = rule.local_address().unwrap_or_default();
        let url = rule.browser_url();

        let open_button = match url.clone() {
            Some(url) => Button::new(("open", rule_id as usize))
                .icon(Icon::new(IconName::ExternalLink))
                .ghost()
                .xsmall()
                .tooltip("Open in browser")
                .on_click(move |_, window, cx| {
                    if let Err(err) = open::that(&url) {
                        window.push_notification(
                            Notification::error(format!("Could not open browser: {err}")),
                            cx,
                        );
                    }
                }),
            None => Button::new(("open", rule_id as usize))
                .icon(Icon::new(IconName::ExternalLink))
                .ghost()
                .xsmall()
                .tooltip("Not bound yet")
                .disabled(true),
        };

        let copy_address = address.clone();
        let copy_button = if copy_address.is_empty() {
            Button::new(("copy", rule_id as usize))
                .icon(Icon::new(IconName::Copy))
                .ghost()
                .xsmall()
                .tooltip("Not bound yet")
                .disabled(true)
        } else {
            let address = copy_address.clone();
            Button::new(("copy", rule_id as usize))
                .icon(Icon::new(IconName::Copy))
                .ghost()
                .xsmall()
                .tooltip("Copy local address")
                .on_click(move |_, window, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(address.clone()));
                    window
                        .push_notification(Notification::success(format!("Copied {address}")), cx);
                })
        };
        let table_for_toggle = table.clone();
        let table_for_remove = table.clone();
        let service_for_toggle = service.clone();
        let service_for_remove = service.clone();

        h_flex()
            .gap_0p5()
            .items_center()
            .px_1()
            .child(open_button)
            .child(copy_button)
            .child(
                Button::new(("toggle", rule_id as usize))
                    .icon(Icon::empty().path(if running {
                        "icons/pause.svg"
                    } else {
                        "icons/play.svg"
                    }))
                    .ghost()
                    .xsmall()
                    .tooltip(if running {
                        "Stop forwarding"
                    } else {
                        "Start forwarding"
                    })
                    .on_click(move |_, window, cx| {
                        let result = if running {
                            service_for_toggle.stop(rule_id);
                            Ok(())
                        } else {
                            service_for_toggle.start(rule_id)
                        };
                        if let Err(err) = result {
                            window.push_notification(Notification::error(format!("{err:#}")), cx);
                        }
                        table_for_toggle.update(cx, |state, cx| {
                            state.delegate_mut().reload();
                            state.refresh(cx);
                        });
                    }),
            )
            .child(
                Button::new(("remove", rule_id as usize))
                    .icon(Icon::empty().path("icons/trash-2.svg"))
                    .ghost()
                    .xsmall()
                    .tooltip("Remove forwarding")
                    .on_click(move |_, _, cx| {
                        service_for_remove.remove_rule(rule_id);
                        table_for_remove.update(cx, |state, cx| {
                            state.delegate_mut().reload();
                            state.refresh(cx);
                        });
                    }),
            )
            .into_any_element()
    }
}

impl TableDelegate for PortsDelegate {
    fn columns_count(&self, _cx: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _cx: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _cx: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn render_th(
        &mut self,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .px_3()
            .text_xs()
            .font_medium()
            .text_color(cx.theme().muted_foreground)
            .child(self.columns[col_ix].name.clone())
    }

    fn render_tr(
        &mut self,
        row_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let active = self
            .rows
            .get(row_ix)
            .map(|row| row.rule.state.is_active())
            .unwrap_or(false);
        div().id(("port-row", row_ix)).when(active, |this| {
            this.bg(cx.theme().table_active.opacity(0.22))
        })
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let Some(row) = self.row(row_ix) else {
            return div().into_any_element();
        };
        match col_ix {
            0 => self.state_cell(&row, cx),
            1 => self.mapping_cell(&row, cx),
            2 => self.process_cell(&row, cx),
            3 => self.visibility_cell(&row, cx),
            4 => self.activity_cell(&row, cx),
            5 => self.actions_cell(&row, cx),
            _ => div().into_any_element(),
        }
    }

    fn render_empty(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        v_flex()
            .size_full()
            .items_center()
            .justify_center()
            .gap_2()
            .child(
                Icon::new(IconName::Inbox)
                    .size_8()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(
                div()
                    .text_sm()
                    .font_medium()
                    .child("No forwarded ports yet"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Add one below with 3000, 8080:3000 or host:5432->15432"),
            )
            .into_any_element()
    }
}
