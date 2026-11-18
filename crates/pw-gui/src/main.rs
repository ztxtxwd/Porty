#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

//! Porty desktop entry point.
//!
//! Owns the Tokio runtime that powers `pw-core` and bridges it into GPUI's
//! foreground executor. The runtime is moved into the application callback so
//! it stays alive for the entire UI session.

mod app;
mod ports_view;
mod theme;

use std::borrow::Cow;

use anyhow::Result;
use gpui::{
    px, size, AppContext as _, Application, AssetSource, Bounds, SharedString, WindowBounds,
    WindowOptions,
};
use gpui_component::{Root, TitleBar};
use pw_core::{config, Porty};

/// Embedded Lucide icon set, keyed by gpui-component's `icons/<name>.svg` paths.
struct Assets;

macro_rules! embedded_icons {
    ($($name:literal),* $(,)?) => {
        fn embedded_icon(path: &str) -> Option<&'static [u8]> {
            match path {
                $(
                    concat!("icons/", $name, ".svg") => {
                        Some(include_bytes!(concat!(
                            "../../../assets/icons/",
                            $name,
                            ".svg"
                        )) as &'static [u8])
                    }
                )*
                _ => None,
            }
        }
    };
}

embedded_icons![
    "arrow-down",
    "arrow-left",
    "arrow-right",
    "arrow-up",
    "check",
    "chevron-down",
    "chevron-left",
    "chevron-right",
    "chevron-up",
    "chevrons-up-down",
    "circle-check",
    "circle-x",
    "close",
    "copy",
    "ellipsis",
    "ellipsis-vertical",
    "external-link",
    "globe",
    "inbox",
    "info",
    "loader-circle",
    "minus",
    "moon",
    "pause",
    "play",
    "plus",
    "refresh-cw",
    "search",
    "settings",
    "settings-2",
    "sort-ascending",
    "sort-descending",
    "sun",
    "trash-2",
    "triangle-alert",
    "user",
];

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(embedded_icon(path).map(Cow::Borrowed))
    }

    fn list(&self, _path: &str) -> Result<Vec<SharedString>> {
        // Icons are addressed directly by gpui-component; no directory listing
        // is needed at runtime.
        Ok(Vec::new())
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "porty=info,pw_core=info,info".into()),
        )
        .init();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("porty-net")
        .enable_all()
        .build()
        .expect("failed to start the Porty network runtime");

    let settings_path = config::default_settings_path();
    let service = runtime
        .block_on(async { Porty::open(tokio::runtime::Handle::current(), settings_path) })
        .expect("failed to initialize Porty");

    let application = Application::new().with_assets(Assets);
    let theme_preference = service.theme_preference();

    application.run(move |cx| {
        // Keep the Tokio runtime alive for as long as the UI runs.
        let _runtime = &runtime;

        gpui_component::init(cx);
        theme::apply_at_startup(theme_preference, cx);
        app::init(cx);

        let bounds = Bounds::centered(None, size(px(1200.), px(780.)), cx);
        #[allow(unused_mut)]
        let mut options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitleBar::title_bar_options()),
            window_min_size: Some(size(px(940.), px(560.))),
            app_id: Some("porty".to_string()),
            ..Default::default()
        };
        // On Linux the custom TitleBar draws the window controls itself.
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            options.window_decorations = Some(gpui::WindowDecorations::Client);
        }

        let service = service.clone();
        cx.spawn(async move |cx| {
            cx.open_window(options, move |window, cx| {
                let service = service.clone();
                let view = cx.new(|cx| app::PortyApp::new(service, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
