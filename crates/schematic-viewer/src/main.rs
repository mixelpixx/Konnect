//! Konnect schematic viewer entry point.
//!
//! The renderer is selected at compile time. Keeping the backends mutually
//! exclusive prevents the native Vello binary from pulling WebKit/WebView
//! dependencies, and keeps the compatibility build free of GPU-specific
//! startup requirements.

#![cfg_attr(
    all(
        feature = "renderer-kicad-cli",
        not(debug_assertions),
        target_os = "windows"
    ),
    windows_subsystem = "windows"
)]

#[cfg(all(feature = "renderer-kicad-cli", feature = "renderer-vello"))]
compile_error!(
    "renderer-kicad-cli and renderer-vello are mutually exclusive; build with exactly one"
);

#[cfg(not(any(feature = "renderer-kicad-cli", feature = "renderer-vello")))]
compile_error!("enable renderer-kicad-cli or renderer-vello");

mod svg_order_cache;
#[cfg(feature = "renderer-kicad-cli")]
mod webview;

#[cfg(feature = "renderer-vello")]
mod change_timeline;
#[cfg(feature = "renderer-vello")]
mod compat_svg;
#[cfg(feature = "renderer-vello")]
mod edit_session;
#[cfg(feature = "renderer-vello")]
mod editor_history;
#[cfg(feature = "renderer-vello")]
mod editor_model;
#[cfg(feature = "renderer-vello")]
mod kicad_font;
#[cfg(feature = "renderer-vello")]
mod kicad_rtree;
#[cfg(feature = "renderer-vello")]
mod native_scene;
#[cfg(feature = "renderer-vello")]
mod vello_app;
#[cfg(feature = "renderer-vello")]
mod vello_render;
#[cfg(feature = "renderer-vello")]
mod viewer_settings;

fn main() {
    #[cfg(feature = "renderer-kicad-cli")]
    webview::run();

    #[cfg(feature = "renderer-vello")]
    if let Err(error) = vello_app::run() {
        eprintln!("schematic viewer failed: {error}");
        std::process::exit(1);
    }
}
