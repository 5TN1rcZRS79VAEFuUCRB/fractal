#![doc(
    html_logo_url = "https://gitlab.gnome.org/World/fractal/-/raw/main/data/icons/org.gnome.Fractal.svg?inline=false",
    html_favicon_url = "https://gitlab.gnome.org/World/fractal/-/raw/main/data/icons/org.gnome.Fractal-symbolic.svg?inline=false"
)]
#![recursion_limit = "256"]
// No console window next to the app on Windows. Output still goes where it is redirected.
#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

// Windows and macOS have no glycin: use the stand-in with the same API.
#[cfg(not(target_os = "linux"))]
extern crate glycin_gtk4_shim as glycin_gtk4;
#[cfg(not(target_os = "linux"))]
extern crate glycin_shim as glycin;

mod account_chooser_dialog;
mod account_switcher;
mod application;
mod components;
#[rustfmt::skip]
mod config;
mod account_settings;
mod contrib;
mod error_page;
mod i18n;
mod identity_verification_view;
mod intent;
mod login;
mod prelude;
mod secret;
mod session;
mod session_list;
mod session_view;
mod system_settings;
#[cfg(any(not(target_os = "linux"), test))]
mod update_check;
mod user_facing_error;
mod utils;
mod window;

use std::sync::LazyLock;

use gettextrs::*;
use gtk::{IconTheme, gdk::Display, gio};
use tracing::warn;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use self::{application::*, config::*, i18n::*, utils::OneshotNotifier, window::Window};

/// The default tokio runtime to be used for async tasks
static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Runtime::new().expect("creating tokio runtime should succeed")
});

/// The notifier to make sure that only one `GtkMediaFile` is played at a single
/// time.
static MEDIA_FILE_NOTIFIER: LazyLock<OneshotNotifier> =
    LazyLock::new(|| OneshotNotifier::new("MEDIA_FILE_NOTIFIER"));

fn main() {
    #[cfg(not(target_os = "linux"))]
    use_bundled_data();

    // Initialize logger, debug is carried out via debug!, info!, warn! and error!.
    // Default to the INFO level for this crate and WARN for everything else.
    // It can be overridden with the RUST_LOG environment variable.
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("fractal=info,warn"));

    tracing_subscriber::registry()
        .with(fmt::layer().with_filter(env_filter))
        .init();

    // Several crypto providers are enabled by our dependencies, so rustls cannot pick one for the
    // connections that do not choose one themselves, like the LiveKit signalling for calls.
    if rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .is_err()
    {
        warn!("A TLS crypto provider was already installed");
    }

    // Prepare i18n
    // Safety: `setlocale` is safe to call because the program is single-threaded.
    #[cfg(target_os = "linux")]
    unsafe {
        setlocale(LocaleCategory::LcAll, "")
    };
    // gettext-rs uses the value of `LC_ALL` of glibc, which the C library of
    // Windows rejects by aborting, so call it with the right value there.
    #[cfg(not(target_os = "linux"))]
    {
        unsafe extern "C" {
            fn setlocale(
                category: std::ffi::c_int,
                locale: *const std::ffi::c_char,
            ) -> *mut std::ffi::c_char;
        }
        /// `LC_ALL` on Windows and macOS.
        const LC_ALL: std::ffi::c_int = 0;
        // Safety: the program is single-threaded, and the locale is a valid C
        // string.
        unsafe { setlocale(LC_ALL, c"".as_ptr()) };
    }
    bindtextdomain(GETTEXT_PACKAGE, data_path(LOCALEDIR))
        .expect("Invalid argument passed to bindtextdomain");
    textdomain(GETTEXT_PACKAGE).expect("Invalid string passed to textdomain");

    gtk::glib::set_application_name("Fractal+");

    gtk::init().expect("Could not start GTK4");
    gst::init().expect("Could not initialize gst");

    #[cfg(target_os = "linux")]
    aperture::init(APP_ID);

    let res =
        gio::Resource::load(data_path(RESOURCES_FILE)).expect("Could not load gresource file");
    gio::resources_register(&res);
    let ui_res = gio::Resource::load(data_path(UI_RESOURCES_FILE))
        .expect("Could not load UI gresource file");
    gio::resources_register(&ui_res);

    IconTheme::for_display(&Display::default().unwrap())
        .add_resource_path("/org/gnome/Fractal/icons");

    let app = Application::new();
    app.run();
}

/// Point GLib, GTK and `GStreamer` to the data and plugins next to the
/// executable, in the Windows and macOS packages.
///
/// Windows finds most of them next to its libraries already, but macOS only
/// looks in the system folders.
#[cfg(not(target_os = "linux"))]
fn use_bundled_data() {
    use std::{env, ffi::OsString};

    let Some(prefix) = env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.to_owned()))
    else {
        return;
    };
    let share = prefix.join("share");
    if !share.join("fractalplus").is_dir() {
        // Not a package, e.g. run from the build folder.
        return;
    }

    let mut vars: Vec<(&str, OsString)> = Vec::new();
    // Icons and settings schemas.
    let mut data_dirs = vec![share.clone()];
    data_dirs.extend(
        env::var_os("XDG_DATA_DIRS")
            .iter()
            .flat_map(env::split_paths),
    );
    if let Ok(data_dirs) = env::join_paths(data_dirs) {
        vars.push(("XDG_DATA_DIRS", data_dirs));
    }
    vars.push((
        "GSETTINGS_SCHEMA_DIR",
        share.join("glib-2.0").join("schemas").into(),
    ));
    let plugins = prefix.join("lib").join("gstreamer-1.0");
    if plugins.is_dir() {
        vars.push(("GST_PLUGIN_SYSTEM_PATH_1_0", plugins.into()));
        let scanner = prefix
            .join("libexec")
            .join("gstreamer-1.0")
            .join(if cfg!(windows) {
                "gst-plugin-scanner.exe"
            } else {
                "gst-plugin-scanner"
            });
        if scanner.is_file() {
            vars.push(("GST_PLUGIN_SCANNER_1_0", scanner.into()));
        } else {
            // Look at the plugins in this process instead.
            vars.push(("GST_REGISTRY_FORK", "no".into()));
        }
    }
    let loaders = prefix.join("lib").join("gdk-pixbuf-2.0").join("2.10.0");
    if loaders.join("loaders.cache").is_file() {
        vars.push(("GDK_PIXBUF_MODULEDIR", loaders.join("loaders").into()));
        vars.push((
            "GDK_PIXBUF_MODULE_FILE",
            loaders.join("loaders.cache").into(),
        ));
    }

    for (name, value) in vars {
        if name == "XDG_DATA_DIRS" || env::var_os(name).is_none() {
            // Safety: nothing else runs yet, so nothing reads the environment
            // at the same time.
            unsafe { env::set_var(name, value) };
        }
    }
}

/// Resolve a data path from the build configuration.
///
/// The Windows and macOS packages use paths relative to the folder above the
/// one with the executable, so they work wherever they are installed.
fn data_path(path: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(path);
    if path.is_absolute() {
        return path.to_owned();
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| Some(exe.parent()?.parent()?.join(path)))
        .unwrap_or_else(|| path.to_owned())
}
