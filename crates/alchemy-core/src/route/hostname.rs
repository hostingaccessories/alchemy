//! Hostnames: app names as DNS labels, and matching a request's `Host`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Longest DNS label, in bytes (RFC 1035 section 2.3.4).
pub const MAX_LABEL_LEN: usize = 63;

/// One DNS label, such as `wiki` in `wiki.localhost`.
///
/// Lowercase ASCII letters, digits, and `-`, 1 to [`MAX_LABEL_LEN`] bytes,
/// never starting or ending with `-`. Serializes as its text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostLabel(String);

/// Why a candidate [`HostLabel`] was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HostLabelError {
    /// The label was empty.
    #[error("a hostname can't be empty")]
    Empty,
    /// The label was longer than [`MAX_LABEL_LEN`] bytes.
    #[error("hostname {0:?} is longer than 63 characters")]
    TooLong(String),
    /// The label held a character outside `[a-z0-9-]`, such as a dot.
    #[error("hostname {0:?} may only use letters, digits, and -, such as wiki")]
    InvalidChar(String),
    /// The label started or ended with `-`.
    #[error("hostname {0:?} can't start or end with -")]
    EdgeHyphen(String),
}

impl HostLabel {
    /// Validate `label`, folding ASCII uppercase to lowercase.
    pub fn new(label: &str) -> Result<Self, HostLabelError> {
        let label = label.to_ascii_lowercase();
        if label.is_empty() {
            return Err(HostLabelError::Empty);
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(HostLabelError::TooLong(label));
        }
        if !label.bytes().all(is_label_byte) {
            return Err(HostLabelError::InvalidChar(label));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(HostLabelError::EdgeHyphen(label));
        }
        Ok(Self(label))
    }

    /// The label for an app named `app`, or `None` when nothing usable is
    /// left.
    ///
    /// ASCII is lowercased, `_` and any other character a label can't hold
    /// become `-`, leading and trailing `-` are trimmed, and the result is
    /// cut to [`MAX_LABEL_LEN`] bytes and trimmed again.
    pub fn from_app(app: &str) -> Option<Self> {
        let mapped: String = app
            .chars()
            .map(|c| {
                let c = c.to_ascii_lowercase();
                if c.is_ascii() && is_label_byte(c as u8) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let trimmed = mapped.trim_matches('-');
        let cut = trimmed.get(..MAX_LABEL_LEN).unwrap_or(trimmed);
        Self::new(cut.trim_end_matches('-')).ok()
    }

    /// The label's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The full hostname under `suffix`, such as `wiki.localhost`.
    pub fn host(&self, suffix: HostSuffix) -> String {
        format!("{}.{}", self.0, suffix.as_str())
    }
}

fn is_label_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'
}

impl fmt::Display for HostLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for HostLabel {
    type Err = HostLabelError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl TryFrom<String> for HostLabel {
    type Error = HostLabelError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<HostLabel> for String {
    fn from(label: HostLabel) -> Self {
        label.0
    }
}

/// A domain apps answer under on port 80.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum HostSuffix {
    /// `localhost`, which browsers resolve to loopback on their own
    /// (RFC 6761 section 6.3).
    Localhost,
    /// `local`, announced over mDNS so other devices can reach it (M7).
    Local,
}

impl HostSuffix {
    /// The suffix without a leading dot.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Localhost => "localhost",
            Self::Local => "local",
        }
    }
}

impl fmt::Display for HostSuffix {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The label a request's `Host` header asks for, lowercased, when it is
/// `<label>.<suffix>` for one of `suffixes`.
///
/// An optional `:port` and one trailing dot are ignored, and ASCII case
/// is folded. IP literals, bare suffixes, and deeper names such as
/// `a.wiki.localhost` match nothing.
pub fn match_host(host: &str, suffixes: &[HostSuffix]) -> Option<String> {
    let name = strip_port(host.trim())?;
    let name = name.strip_suffix('.').unwrap_or(name);
    suffixes.iter().find_map(|suffix| {
        let label = strip_suffix_ignore_case(name, suffix.as_str())?.strip_suffix('.')?;
        (!label.is_empty() && !label.contains('.')).then(|| label.to_ascii_lowercase())
    })
}

fn strip_port(host: &str) -> Option<&str> {
    if host.starts_with('[') {
        return None;
    }
    match host.rsplit_once(':') {
        Some((name, port)) if port.bytes().all(|b| b.is_ascii_digit()) => Some(name),
        Some(_) => None,
        None => Some(host),
    }
}

fn strip_suffix_ignore_case<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let split = s.len().checked_sub(suffix.len())?;
    let (head, tail) = (s.get(..split)?, s.get(split..)?);
    tail.eq_ignore_ascii_case(suffix).then_some(head)
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
    fn label_validation() {
        let ok = [("wiki", "wiki"), ("Wiki-2", "wiki-2"), ("a", "a")];
        for (input, want) in ok {
            assert_eq!(HostLabel::new(input).unwrap().as_str(), want, "{input}");
        }
        let long = "a".repeat(64);
        let bad = [
            ("", HostLabelError::Empty),
            (long.as_str(), HostLabelError::TooLong(long.clone())),
            ("my_app", HostLabelError::InvalidChar("my_app".into())),
            (
                "wiki.localhost",
                HostLabelError::InvalidChar("wiki.localhost".into()),
            ),
            ("wïki", HostLabelError::InvalidChar("wïki".into())),
            ("-wiki", HostLabelError::EdgeHyphen("-wiki".into())),
            ("wiki-", HostLabelError::EdgeHyphen("wiki-".into())),
        ];
        for (input, want) in bad {
            assert_eq!(HostLabel::new(input), Err(want), "{input}");
        }
        assert!(HostLabel::new(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn label_from_app_name() {
        let long = format!("{}_b", "a".repeat(62));
        let cases = [
            ("wiki", Some("wiki")),
            ("my_app", Some("my-app")),
            ("_my_app_", Some("my-app")),
            ("--a--", Some("a")),
            ("Photos", Some("photos")),
            ("a.b", Some("a-b")),
            ("___", None),
            ("", None),
            (long.as_str(), Some(&long[..62])),
        ];
        for (app, want) in cases {
            assert_eq!(
                HostLabel::from_app(app).as_ref().map(HostLabel::as_str),
                want,
                "{app}"
            );
        }
        let exact = "b".repeat(70);
        assert_eq!(HostLabel::from_app(&exact).unwrap().as_str().len(), 63);
    }

    #[test]
    fn label_serde_and_host() {
        let label = HostLabel::new("wiki").unwrap();
        assert_eq!(serde_json::to_string(&label).unwrap(), "\"wiki\"");
        assert_eq!(
            serde_json::from_str::<HostLabel>("\"Wiki\"").unwrap(),
            label
        );
        assert!(serde_json::from_str::<HostLabel>("\"a.b\"").is_err());
        assert_eq!(label.host(HostSuffix::Localhost), "wiki.localhost");
        assert_eq!(label.host(HostSuffix::Local), "wiki.local");
    }

    #[test]
    fn host_matching() {
        use HostSuffix::{Local, Localhost};
        let both = [Localhost, Local];
        let cases: &[(&str, &[HostSuffix], Option<&str>)] = &[
            ("wiki.localhost", &[Localhost], Some("wiki")),
            ("WIKI.LocalHost", &[Localhost], Some("wiki")),
            ("wiki.localhost:80", &[Localhost], Some("wiki")),
            ("wiki.localhost:8080", &[Localhost], Some("wiki")),
            ("wiki.localhost.", &[Localhost], Some("wiki")),
            ("wiki.localhost.:80", &[Localhost], Some("wiki")),
            (" wiki.localhost ", &[Localhost], Some("wiki")),
            ("wiki.local", &[Localhost], None),
            ("wiki.local", &both, Some("wiki")),
            ("wiki.localhost", &both, Some("wiki")),
            ("my-app.local:80", &both, Some("my-app")),
            ("localhost", &both, None),
            (".localhost", &both, None),
            ("a.wiki.localhost", &both, None),
            ("wikilocalhost", &both, None),
            ("wiki.localhost:http", &both, None),
            ("127.0.0.1", &both, None),
            ("127.0.0.1:80", &both, None),
            ("[::1]:80", &both, None),
            ("", &both, None),
            ("wiki.localhost", &[], None),
        ];
        for (host, suffixes, want) in cases {
            assert_eq!(
                match_host(host, suffixes).as_deref(),
                *want,
                "{host} {suffixes:?}"
            );
        }
    }
}
