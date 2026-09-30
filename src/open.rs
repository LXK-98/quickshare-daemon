// SPDX-License-Identifier: GPL-3.0-or-later
//! Opening received files, and revealing them in the file manager.
//!
//! Both actions are reached from the notification posted when a transfer
//! finishes. Revealing goes through `org.freedesktop.FileManager1`, which
//! Nautilus implements: it opens the containing folder *with the files
//! selected*, which is what "show in folder" means to anyone who asks for it.
//! Opening goes through `xdg-open`, since we are not sandboxed and so have no
//! reason to route it via a portal.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

/// File extensions that `xdg-open` would *execute* rather than display.
///
/// A received file's name is chosen entirely by the sender. `xdg-open` on a
/// `.desktop` file launches it, so without this an attacker on the same
/// network gets from "you accepted a file" to "you ran my code" with one click
/// on a notification button — and the button would be sitting under a name the
/// sender also chose, like `Invoice.pdf.desktop`.
///
/// Refusing to open these is not much protection on its own (the file is
/// already on disk), but the notification button should not be the thing that
/// runs it.
const NEVER_OPEN: [&str; 6] = ["desktop", "sh", "bash", "run", "appimage", "bin"];

/// Whether [`open_file`] will refuse this path.
fn is_executable_kind(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| NEVER_OPEN.contains(&ext.as_str()))
}

#[zbus::proxy(
    interface = "org.freedesktop.FileManager1",
    default_service = "org.freedesktop.FileManager1",
    default_path = "/org/freedesktop/FileManager1"
)]
trait FileManager {
    /// Opens the containing folders with these items selected.
    fn show_items(&self, uris: &[&str], startup_id: &str) -> zbus::Result<()>;

    /// Opens these folders themselves.
    fn show_folders(&self, uris: &[&str], startup_id: &str) -> zbus::Result<()>;
}

/// Opens the file manager with `paths` selected.
///
/// Falls back to opening the containing directory if no `FileManager1`
/// implementation is running — the selection is a nicety, actually getting to
/// the folder is the point.
pub async fn reveal(paths: &[PathBuf]) -> Result<()> {
    let Some(first) = paths.first() else {
        bail!("nothing to reveal");
    };

    let uris: Vec<String> = paths.iter().map(|p| file_uri(p)).collect();
    let uri_refs: Vec<&str> = uris.iter().map(String::as_str).collect();

    let connection = zbus::Connection::session()
        .await
        .context("connecting to the session bus")?;

    match FileManagerProxy::new(&connection).await {
        Ok(proxy) => proxy
            .show_items(&uri_refs, "")
            .await
            .context("asking the file manager to show the files"),
        Err(err) => {
            tracing::debug!("no FileManager1 implementation ({err}); opening the folder instead");
            let parent = first
                .parent()
                .context("the received file has no containing directory")?;
            spawn_xdg_open(parent).await
        }
    }
}

/// Opens a received file with its default application.
///
/// Refuses the handful of extensions that `xdg-open` would execute; see
/// [`NEVER_OPEN`]. The caller should fall back to [`reveal`], which is safe for
/// anything.
pub async fn open_file(path: &Path) -> Result<()> {
    if is_executable_kind(path) {
        bail!(
            "refusing to open {}: a sender-named file of this kind would be executed, not displayed",
            path.display()
        );
    }
    spawn_xdg_open(path).await
}

/// Runs `xdg-open` on a path and detaches from it.
///
/// The child is reaped by tokio rather than left as a zombie, but nothing waits
/// for it: `xdg-open` typically hands off to an already-running application and
/// exits, and a file manager or video player outliving the daemon is normal.
async fn spawn_xdg_open(path: &Path) -> Result<()> {
    tokio::process::Command::new("xdg-open")
        .arg(path)
        // Anything it prints belongs in its own diagnostics, not in our journal.
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false)
        .spawn()
        .with_context(|| format!("running xdg-open on {}", path.display()))?;
    Ok(())
}

/// Builds a `file://` URI, percent-encoding what must be encoded.
///
/// Written out rather than pulled from a crate because the rules for a path are
/// narrow: everything outside the unreserved set gets encoded, `/` stays as the
/// separator. Received filenames come from the sender and routinely contain
/// spaces, `#`, `?` and non-ASCII — all of which break a naive `format!`.
fn file_uri(path: &Path) -> String {
    const UNRESERVED_EXTRA: [u8; 5] = [b'-', b'_', b'.', b'~', b'/'];

    let mut uri = String::from("file://");
    for byte in path.to_string_lossy().as_bytes() {
        if byte.is_ascii_alphanumeric() || UNRESERVED_EXTRA.contains(byte) {
            uri.push(*byte as char);
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_file_uris() {
        assert_eq!(
            file_uri(Path::new("~/Downloads/photo.jpg")),
            "file://~/Downloads/photo.jpg"
        );
    }

    #[test]
    fn encodes_what_a_sender_might_name_a_file() {
        // Spaces are the common case.
        assert_eq!(
            file_uri(Path::new("/tmp/my holiday.jpg")),
            "file:///tmp/my%20holiday.jpg"
        );
        // `#` would otherwise start a fragment and truncate the path.
        assert_eq!(file_uri(Path::new("/tmp/a#b")), "file:///tmp/a%23b");
        // `?` would otherwise start a query.
        assert_eq!(file_uri(Path::new("/tmp/a?b")), "file:///tmp/a%3Fb");
        // Non-ASCII is percent-encoded per UTF-8 byte.
        assert_eq!(file_uri(Path::new("/tmp/é")), "file:///tmp/%C3%A9");
        // Unreserved characters are left alone.
        assert_eq!(
            file_uri(Path::new("/tmp/a-b_c.d~e")),
            "file:///tmp/a-b_c.d~e"
        );
    }

    #[test]
    fn refuses_to_open_what_would_execute() {
        for name in [
            "Invoice.pdf.desktop",
            "setup.sh",
            "thing.AppImage",
            "SCRIPT.SH",
            "installer.run",
        ] {
            assert!(
                is_executable_kind(Path::new(name)),
                "{name} should not be openable"
            );
        }
    }

    #[test]
    fn ordinary_files_are_openable() {
        for name in [
            "photo.jpg",
            "clip.mp4",
            "notes.txt",
            "doc.pdf",
            "no-extension",
        ] {
            assert!(!is_executable_kind(Path::new(name)), "{name} should open");
        }
    }
}
