//! Opening a sign-in link in the person's web browser, for `alchemy open`.
//!
//! The link carries a one-time launch code, and a process's arguments can
//! be read by every user on the machine (`ps`, `/proc/PID/cmdline`), so
//! the link is never put on a command line: another user could read the
//! code and redeem it before the browser does. Instead it goes in a small
//! page, readable only by this user, that sends the browser on to it, and
//! the opener is given that file. Browsers confined away from the runtime
//! directory, such as Ubuntu's Firefox snap, can't read it; for those,
//! `alchemy open --print` prints the link to paste.

use std::ffi::OsStr;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The page that sends the browser on, inside the runtime directory.
pub const REDIRECT_FILE: &str = "open.html";

/// How long to wait for the opener to say whether it worked. One still
/// running after this, such as `xdg-open` starting a browser in the
/// foreground, is taken to have worked.
const OPENER_WAIT: Duration = Duration::from_secs(3);

/// Where a command opens links: the system's browser in the binary, a
/// recorder in tests.
pub trait Browser {
    /// Open `url`, or say why it could not be.
    fn open(&mut self, url: &str) -> impl Future<Output = Result<(), String>>;
}

/// The operating systems with an opener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    allow(dead_code, reason = "only the host's variant is built outside tests")
)]
pub(crate) enum Os {
    Linux,
    MacOs,
}

#[cfg(target_os = "macos")]
const OS: Os = Os::MacOs;

#[cfg(not(target_os = "macos"))]
const OS: Os = Os::Linux;

/// The program that opens files in their default app on `os`, by full
/// path: macOS's own `open`, or `xdg-open` from the first absolute
/// directory in `path` that `exists` says has it. Relative `PATH` entries
/// are skipped, so nothing in the working directory is run.
pub(crate) fn opener(
    os: Os,
    path: Option<&OsStr>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    match os {
        Os::MacOs => Some(PathBuf::from("/usr/bin/open")).filter(|open| exists(open)),
        Os::Linux => std::env::split_paths(path?)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join("xdg-open"))
            .find(|candidate| exists(candidate)),
    }
}

/// A page that sends the browser straight on to `url`, without a referrer.
pub(crate) fn redirect_page(url: &str) -> String {
    let url = html_escape(url);
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<meta charset=\"utf-8\">\n\
         <meta name=\"referrer\" content=\"no-referrer\">\n\
         <meta http-equiv=\"refresh\" content=\"0;url={url}\">\n\
         <title>Opening Alchemy</title>\n\
         <p><a href=\"{url}\">Open Alchemy</a></p>\n</html>\n"
    )
}

fn html_escape(text: &str) -> String {
    text.chars().fold(String::new(), |mut out, c| {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
        out
    })
}

/// Opens links in the default browser through the system's opener, by
/// way of a private redirect page in `dir`.
#[derive(Debug, Clone)]
pub struct SystemBrowser {
    dir: PathBuf,
}

impl SystemBrowser {
    /// Write redirect pages to `dir`, usually alchemyd's runtime
    /// directory. It is created `0o700` if missing, and refused if anyone
    /// but this user can open it.
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn write_page(&self, url: &str) -> std::io::Result<PathBuf> {
        private_dir(&self.dir)?;
        let mut staged = tempfile::Builder::new()
            .prefix(".open-")
            .suffix(".html")
            .tempfile_in(&self.dir)?;
        staged.write_all(redirect_page(url).as_bytes())?;
        let path = self.dir.join(REDIRECT_FILE);
        staged.persist(&path).map_err(|e| e.error)?;
        Ok(path)
    }
}

impl Browser for SystemBrowser {
    /// Waits for the opener on the runtime, at most [`OPENER_WAIT`].
    fn open(&mut self, url: &str) -> impl Future<Output = Result<(), String>> {
        let prepared = opener(OS, std::env::var_os("PATH").as_deref(), Path::is_file)
            .ok_or_else(|| "no program to open it with was found".to_owned())
            .and_then(|opener| match self.write_page(url) {
                Ok(page) => Ok((opener, page)),
                Err(e) => Err(format!("cannot write the page that opens it: {e}")),
            });
        async move {
            let (opener, page) = prepared?;
            let mut child = std::process::Command::new(&opener)
                .arg(&page)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("cannot run {}: {e}", opener.display()))?;
            let deadline = tokio::time::Instant::now() + OPENER_WAIT;
            while tokio::time::Instant::now() < deadline {
                match child.try_wait() {
                    Ok(Some(status)) if status.success() => return Ok(()),
                    Ok(Some(status)) => {
                        return Err(format!("{} exited with {status}", opener.display()));
                    }
                    Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                    Err(e) => return Err(format!("cannot wait for {}: {e}", opener.display())),
                }
            }
            Ok(())
        }
    }
}

/// Create `dir` `0o700` if missing, and check only this user can open it.
#[cfg(unix)]
fn private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _};

    match std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
    {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let meta = std::fs::symlink_metadata(dir)?;
    let ours = meta.is_dir() && meta.uid() == rustix::process::getuid().as_raw();
    if !ours || meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::other(format!(
            "{} is not a directory only you can open",
            dir.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn private_dir(_dir: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other("unsupported platform"))
}

/// A recording [`Browser`] that answers every open with `answer`.
#[cfg(test)]
#[derive(Debug)]
pub(crate) struct FakeBrowser {
    answer: Result<(), String>,
    opened: Vec<String>,
}

#[cfg(test)]
impl FakeBrowser {
    /// A browser that opens everything.
    pub(crate) fn working() -> Self {
        Self {
            answer: Ok(()),
            opened: Vec::new(),
        }
    }

    /// A browser that fails with `reason`.
    pub(crate) fn failing(reason: &str) -> Self {
        Self {
            answer: Err(reason.to_owned()),
            opened: Vec::new(),
        }
    }

    /// Every URL it was asked to open.
    pub(crate) fn opened(&self) -> &[String] {
        &self.opened
    }
}

#[cfg(test)]
impl Browser for FakeBrowser {
    fn open(&mut self, url: &str) -> impl Future<Output = Result<(), String>> {
        self.opened.push(url.to_owned());
        std::future::ready(self.answer.clone())
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use super::*;

    #[test]
    fn opener_table() {
        let has = |paths: &'static [&'static str]| {
            move |p: &Path| paths.iter().any(|q| p == Path::new(q))
        };
        let cases = [
            (
                "macos",
                Os::MacOs,
                None,
                has(&["/usr/bin/open"]),
                Some("/usr/bin/open"),
            ),
            ("macos without open", Os::MacOs, None, has(&[]), None),
            (
                "linux on PATH",
                Os::Linux,
                Some("/usr/local/bin:/usr/bin"),
                has(&["/usr/bin/xdg-open"]),
                Some("/usr/bin/xdg-open"),
            ),
            (
                "linux first on PATH wins",
                Os::Linux,
                Some("/opt/bin:/usr/bin"),
                has(&["/opt/bin/xdg-open", "/usr/bin/xdg-open"]),
                Some("/opt/bin/xdg-open"),
            ),
            (
                "linux skips relative entries",
                Os::Linux,
                Some("bin:.:/usr/bin"),
                has(&["bin/xdg-open", "./xdg-open", "/usr/bin/xdg-open"]),
                Some("/usr/bin/xdg-open"),
            ),
            (
                "linux without one",
                Os::Linux,
                Some("/usr/bin"),
                has(&[]),
                None,
            ),
            (
                "linux without PATH",
                Os::Linux,
                None,
                has(&["/usr/bin/xdg-open"]),
                None,
            ),
        ];
        for (name, os, path, exists, want) in cases {
            let got = opener(os, path.map(OsStr::new), exists);
            assert_eq!(got, want.map(PathBuf::from), "{name}");
        }
    }

    #[test]
    fn redirect_page_escapes_the_link() {
        let page = redirect_page("http://127.0.0.1:7240/login?code=ab&x=\"<y>'");
        let escaped = "http://127.0.0.1:7240/login?code=ab&amp;x=&quot;&lt;y&gt;&#39;";
        assert!(
            page.contains(&format!("content=\"0;url={escaped}\"")),
            "{page}"
        );
        assert!(page.contains(&format!("href=\"{escaped}\"")), "{page}");
        assert!(page.contains("content=\"no-referrer\""), "{page}");
    }

    #[cfg(unix)]
    #[test]
    fn the_page_is_private_and_replaced() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("runtime");
        let browser = SystemBrowser::new(dir.clone());
        let first = browser
            .write_page("http://127.0.0.1:7240/login?code=1")
            .unwrap();
        let second = browser
            .write_page("http://127.0.0.1:7240/login?code=2")
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(first, dir.join(REDIRECT_FILE));
        let page = std::fs::read_to_string(&second).unwrap();
        assert!(page.contains("code=2"), "{page}");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&second), 0o600);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_others_can_open_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("shared");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let err = SystemBrowser::new(dir.clone())
            .write_page("http://127.0.0.1:7240/login?code=1")
            .unwrap_err();
        assert!(err.to_string().contains("only you"), "{err}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }
}
