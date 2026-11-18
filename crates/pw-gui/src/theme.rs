//! Visual language for Porty.
//!
//! Uses gpui-component's theme tokens so the app follows the same palette as
//! the rest of the GPUI ecosystem, with a few helpers for status colors and
//! human-readable traffic figures.

use gpui::{div, px, App, Div, Hsla, Styled as _, Window};
use gpui_component::theme::{Theme, ThemeMode};
use gpui_component::ActiveTheme as _;
use pw_core::{config::ThemePreference, RuleState, SessionState};

pub const SIDEBAR_WIDTH: f32 = 236.;
/// Apply a theme preference before any window exists.
pub fn apply_at_startup(preference: ThemePreference, cx: &mut App) {
    match preference {
        ThemePreference::System => Theme::sync_system_appearance(None, cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, None, cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, None, cx),
    }
    tune(cx);
}

/// Apply a theme preference to a live window.
pub fn apply(preference: ThemePreference, window: &mut Window, cx: &mut App) {
    match preference {
        ThemePreference::System => Theme::sync_system_appearance(Some(window), cx),
        ThemePreference::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        ThemePreference::Light => Theme::change(ThemeMode::Light, Some(window), cx),
    }
    tune(cx);
    window.refresh();
}

fn tune(cx: &mut App) {
    let theme = Theme::global_mut(cx);
    theme.radius = px(6.);
    theme.radius_lg = px(8.);
}

/// Color of the connection dot for a host.
pub fn session_color(state: &SessionState, cx: &App) -> Hsla {
    let theme = cx.theme();
    match state {
        SessionState::Connected => theme.success,
        SessionState::Connecting | SessionState::Authenticating => theme.warning,
        SessionState::Reconnecting { .. } => theme.warning,
        SessionState::Error { .. } => theme.danger,
        SessionState::Disconnected => theme.muted_foreground,
    }
}

/// Color of the state pill for a forwarding rule.
pub fn rule_color(state: &RuleState, cx: &App) -> Hsla {
    let theme = cx.theme();
    match state {
        RuleState::Active => theme.success,
        RuleState::Starting => theme.warning,
        RuleState::Error { .. } => theme.danger,
        RuleState::Stopped => theme.muted_foreground,
    }
}

/// Small filled circle used for status indicators.
pub fn dot(color: Hsla) -> Div {
    div().size(px(7.)).rounded_full().bg(color).flex_shrink_0()
}

/// Larger status dot with a soft ring, for the sidebar.
pub fn status_dot(color: Hsla) -> Div {
    div()
        .size(px(8.))
        .rounded_full()
        .bg(color)
        .flex_shrink_0()
        .shadow_sm()
}

/// Dense cell text: `2 active · 1.4 MB`, `1.4 MB total`, or `idle`.
pub fn dense_activity(active: usize, total_bytes: u64) -> String {
    if active == 0 && total_bytes == 0 {
        return "idle".to_string();
    }
    if active == 0 {
        return format!("{} total", format_bytes(total_bytes));
    }
    format!("{active} active · {}", format_bytes(total_bytes))
}

/// Human readable byte count: `812 B`, `1.4 KB`, `18.2 MB`, `1.1 GB`.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes} B")
    } else if value < MB {
        format!("{:.1} KB", value / KB)
    } else if value < GB {
        format!("{:.1} MB", value / MB)
    } else {
        format!("{:.1} GB", value / GB)
    }
}

/// Keep long remote command lines to a single readable line.
pub fn truncate(value: &str, max_chars: usize) -> String {
    let value = value.trim();
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut truncated: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    truncated.push('…');
    truncated
}
