//! Tell the user when a newer release of Fractal+ is out, on the systems where
//! no package manager updates it (Windows and macOS).
#![cfg_attr(
    target_os = "linux",
    allow(
        dead_code,
        reason = "only used on Windows and macOS, built on Linux for the tests"
    )
)]

use adw::prelude::*;
use gettextrs::gettext;
use gtk::gio;
use serde::Deserialize;
use tracing::{debug, warn};

use crate::{Window, config::VERSION, gettext_f, spawn, spawn_tokio};

/// The releases of Fractal+, newest first.
///
/// The "latest release" API would skip pre-releases, which all releases are
/// for now.
const RELEASES_API: &str =
    "https://api.github.com/repos/5TN1rcZRS79VAEFuUCRB/fractalplus/releases?per_page=10";

/// A release on GitHub.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
}

/// Check for a newer release in the background, and show a notice with a
/// download link in the given window if there is one.
pub(crate) fn check_for_update(window: &Window) {
    let window = window.downgrade();
    spawn!(async move {
        let handle = spawn_tokio!(fetch_releases());
        let releases = match handle.await.expect("task was not aborted") {
            Ok(releases) => releases,
            Err(error) => {
                debug!("Could not check for a newer release: {error}");
                return;
            }
        };
        let Some((version, url)) = newer_release(VERSION, releases) else {
            return;
        };
        let Some(window) = window.upgrade() else {
            return;
        };
        show_notice(&window, &version, url);
    });
}

/// Get the newest releases from GitHub.
async fn fetch_releases() -> Result<Vec<Release>, Box<dyn std::error::Error + Send + Sync>> {
    let client = matrix_sdk::reqwest::Client::builder()
        // GitHub refuses requests without a user agent.
        .user_agent(format!("Fractal+/{VERSION}"))
        .build()?;
    let bytes = client
        .get(RELEASES_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// The version and page of the newest release, if it is newer than the given
/// version.
fn newer_release(current: &str, releases: Vec<Release>) -> Option<(String, String)> {
    let current = parse_version(current)?;
    releases
        .into_iter()
        .filter(|release| !release.draft)
        .filter_map(|release| {
            let version = parse_version(&release.tag_name)?;
            Some((version, release))
        })
        .filter(|(version, _)| *version > current)
        .max_by(|(a, _), (b, _)| a.cmp(b))
        .map(|(_, release)| {
            let version = release.tag_name.trim_start_matches('v').to_owned();
            (version, release.html_url)
        })
}

/// Parse a version like `14.3.2`, `v14.3.2` or `14.3.2-abcdef` into its
/// numbers.
fn parse_version(version: &str) -> Option<Vec<u64>> {
    let version = version.trim_start_matches('v');
    let version = version.split('-').next()?;
    version
        .split('.')
        .map(|number| number.parse().ok())
        .collect()
}

/// Show a notice that the given version is out, with a button to download
/// it.
fn show_notice(window: &Window, version: &str, url: String) {
    let toast = adw::Toast::builder()
        .title(gettext_f(
            // Translators: Do NOT translate the content between '{' and '}', this is a
            // variable name.
            "Fractal+ {version} is available",
            &[("version", version)],
        ))
        .button_label(gettext("Download"))
        // Stay until the user closes it.
        .timeout(0)
        .build();
    let window_weak = window.downgrade();
    toast.connect_button_clicked(move |_| {
        let window = window_weak.upgrade();
        gtk::UriLauncher::new(&url).launch(window.as_ref(), None::<&gio::Cancellable>, |result| {
            if let Err(error) = result {
                warn!("Could not open the download page: {error}");
            }
        });
    });
    window.add_toast(toast);
    debug!("Fractal+ {version} is available");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.to_owned(),
            html_url: format!("https://example.org/{tag}"),
            draft: false,
        }
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("14.3.2"), Some(vec![14, 3, 2]));
        assert_eq!(parse_version("v14.3"), Some(vec![14, 3]));
        assert_eq!(parse_version("14.3.1-e33074db"), Some(vec![14, 3, 1]));
        assert_eq!(parse_version("nightly"), None);
    }

    #[test]
    fn finds_newer_release() {
        let releases = vec![release("v14.3.1"), release("v14.4"), release("v14.3.2")];
        assert_eq!(
            newer_release("14.3.2", releases),
            Some(("14.4".to_owned(), "https://example.org/v14.4".to_owned()))
        );
    }

    #[test]
    fn ignores_older_and_draft_releases() {
        let mut draft = release("v15.0");
        draft.draft = true;
        let releases = vec![release("v14.3.2"), release("v14.3.1"), draft];
        assert_eq!(newer_release("14.3.2", releases), None);
    }
}
