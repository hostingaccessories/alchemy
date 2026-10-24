//! Parsing `docker compose --progress json` output.
//!
//! Compose writes one JSON object per line. Most are resource events on
//! standard error (`{"id":"Container blog-web-1","status":"Done",
//! "text":"Started"}`); a run that fails ends with
//! `{"error":true,"message":"..."}`; and `build` passes `BuildKit`'s own
//! solve status through on standard output
//! (`{"vertexes":[...],"statuses":[...],"logs":[...]}`), where each log
//! entry carries a chunk of a step's output in base64.
//!
//! [`parse_progress`] is tolerant: unknown fields are ignored, a field of
//! the wrong type reads as missing, and anything that is not one of these
//! shapes, including plain text, is `None`. Callers keep the raw line
//! either way.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value};

/// A recognised progress line.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProgressEvent {
    /// A container, network, volume, or image changed state.
    Resource(ResourceProgress),
    /// A message about the run as a whole, such as the reason it failed.
    Message {
        /// Whether this is the error that failed the run.
        error: bool,
        /// The message text.
        message: String,
    },
    /// `BuildKit` progress from a build.
    Build(BuildProgress),
}

/// How far along a resource is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceStatus {
    /// In progress, such as `Pulling` or `Starting`.
    Working,
    /// Finished, such as `Pulled` or `Started`.
    Done,
    /// Finished with a warning, or interrupted.
    Warning,
    /// Failed.
    Error,
    /// A status this version does not know, kept verbatim.
    Other(String),
}

impl ResourceStatus {
    fn parse(status: &str) -> Self {
        match status {
            "Working" => Self::Working,
            "Done" => Self::Done,
            "Warning" => Self::Warning,
            "Error" => Self::Error,
            other => Self::Other(other.to_owned()),
        }
    }
}

/// One resource event from Compose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceProgress {
    /// The resource, such as `Container blog-web-1` or `Image nginx:1.27`.
    pub id: String,
    /// The resource this one belongs to, such as the image a layer is part
    /// of.
    pub parent_id: Option<String>,
    /// How far along it is.
    pub status: ResourceStatus,
    /// What is happening, such as `Pulling` or `Started`.
    pub text: String,
    /// More detail, such as why it failed.
    pub details: Option<String>,
    /// Bytes or units done so far, when Compose reports them.
    pub current: Option<u64>,
    /// Bytes or units in total, when Compose reports them.
    pub total: Option<u64>,
    /// Percent complete, when Compose reports it.
    pub percent: Option<u8>,
}

/// One `BuildKit` solve status line from a build.
///
/// The build steps and their logs are kept; transfer statuses and
/// warnings stay in the raw line.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BuildProgress {
    /// The build steps this line reports on. Empty for a line that only
    /// carries logs or transfer statuses.
    pub vertexes: Vec<BuildVertex>,
    /// What the build steps wrote, in order. A log entry whose data is not
    /// base64 is left out.
    pub logs: Vec<VertexLog>,
}

/// A chunk of one `BuildKit` build step's output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VertexLog {
    /// The digest of the step that wrote it.
    pub vertex: String,
    /// The step's stream: 1 for standard output, 2 for standard error.
    pub stream: u8,
    /// The bytes written, which need not end at a line.
    pub data: Vec<u8>,
}

/// One `BuildKit` build step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildVertex {
    /// The step's digest, which identifies it across lines.
    pub digest: String,
    /// What the step does, such as `[2/3] RUN npm ci`.
    pub name: String,
    /// Whether the step has started.
    pub started: bool,
    /// Whether the step has finished.
    pub completed: bool,
    /// Whether the step was a cache hit.
    pub cached: bool,
    /// Why the step failed.
    pub error: Option<String>,
}

const BUILD_KEYS: [&str; 4] = ["vertexes", "statuses", "logs", "warnings"];

/// Parse one line of `docker compose --progress json` output.
pub fn parse_progress(line: &str) -> Option<ProgressEvent> {
    let line = line.trim();
    if !line.starts_with('{') {
        return None;
    }
    let Ok(Value::Object(object)) = serde_json::from_str::<Value>(line) else {
        return None;
    };
    if let Some(id) = string(&object, "id") {
        return resource(&object, id);
    }
    if let Some(message) = string(&object, "message") {
        return Some(ProgressEvent::Message {
            error: object
                .get("error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            message,
        });
    }
    if object.get("tail").and_then(Value::as_bool) == Some(true) {
        return string(&object, "text").map(|message| ProgressEvent::Message {
            error: false,
            message,
        });
    }
    if BUILD_KEYS.iter().any(|key| object.contains_key(*key)) {
        return Some(ProgressEvent::Build(build(&object)));
    }
    None
}

fn resource(object: &Map<String, Value>, id: String) -> Option<ProgressEvent> {
    let status = string(object, "status");
    let text = string(object, "text");
    if status.is_none() && text.is_none() {
        return None;
    }
    Some(ProgressEvent::Resource(ResourceProgress {
        id,
        parent_id: string(object, "parent_id"),
        status: status.map_or(ResourceStatus::Working, |s| ResourceStatus::parse(&s)),
        text: text.unwrap_or_default(),
        details: string(object, "details"),
        current: object.get("current").and_then(Value::as_u64),
        total: object.get("total").and_then(Value::as_u64),
        percent: object
            .get("percent")
            .and_then(Value::as_u64)
            .and_then(|p| u8::try_from(p).ok())
            .filter(|p| *p <= 100),
    }))
}

fn build(object: &Map<String, Value>) -> BuildProgress {
    let vertexes = object
        .get("vertexes")
        .and_then(Value::as_array)
        .map(|vertexes| {
            vertexes
                .iter()
                .filter_map(Value::as_object)
                .filter_map(vertex)
                .collect()
        })
        .unwrap_or_default();
    let logs = object
        .get("logs")
        .and_then(Value::as_array)
        .map(|logs| {
            logs.iter()
                .filter_map(Value::as_object)
                .filter_map(vertex_log)
                .collect()
        })
        .unwrap_or_default();
    BuildProgress { vertexes, logs }
}

/// A log entry, read as standard output when its stream is missing.
fn vertex_log(object: &Map<String, Value>) -> Option<VertexLog> {
    Some(VertexLog {
        vertex: string(object, "vertex")?,
        stream: object
            .get("stream")
            .and_then(Value::as_u64)
            .and_then(|s| u8::try_from(s).ok())
            .unwrap_or(1),
        data: STANDARD.decode(string(object, "data")?).ok()?,
    })
}

fn vertex(object: &Map<String, Value>) -> Option<BuildVertex> {
    Some(BuildVertex {
        digest: string(object, "digest")?,
        name: string(object, "name").unwrap_or_default(),
        started: object.get("started").is_some_and(|v| !v.is_null()),
        completed: object.get("completed").is_some_and(|v| !v.is_null()),
        cached: object
            .get("cached")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        error: string(object, "error"),
    })
}

/// A non-empty string field.
fn string(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
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

    fn resource(id: &str, status: ResourceStatus, text: &str) -> ResourceProgress {
        ResourceProgress {
            id: id.to_owned(),
            parent_id: None,
            status,
            text: text.to_owned(),
            details: None,
            current: None,
            total: None,
            percent: None,
        }
    }

    #[test]
    fn parses_resource_events() {
        let cases = [
            (
                r#"{"id":"Container exp-web-1","status":"Working","text":"Restarting"}"#,
                resource("Container exp-web-1", ResourceStatus::Working, "Restarting"),
            ),
            (
                r#"{"id":"Network exp_default","status":"Done","text":"Removed"}"#,
                resource("Network exp_default", ResourceStatus::Done, "Removed"),
            ),
            (
                r#"{"id":"Image busybox:1.36","status":"Warning","text":"Interrupted"}"#,
                resource("Image busybox:1.36", ResourceStatus::Warning, "Interrupted"),
            ),
            (
                r#"{"id":"built","status":"Done","text":"Skipped","details":"No image to be pulled"}"#,
                ResourceProgress {
                    details: Some("No image to be pulled".to_owned()),
                    ..resource("built", ResourceStatus::Done, "Skipped")
                },
            ),
            (
                r#"{"id":"Image nope:1","status":"Error","text":"Error","details":"pull access denied"}"#,
                ResourceProgress {
                    details: Some("pull access denied".to_owned()),
                    ..resource("Image nope:1", ResourceStatus::Error, "Error")
                },
            ),
            (
                r#"{"id":"a1b2","parent_id":"Image nginx:1.27","status":"Working","text":"Downloading","current":1024,"total":4096,"percent":25}"#,
                ResourceProgress {
                    parent_id: Some("Image nginx:1.27".to_owned()),
                    current: Some(1024),
                    total: Some(4096),
                    percent: Some(25),
                    ..resource("a1b2", ResourceStatus::Working, "Downloading")
                },
            ),
            (
                r#"{"id":"x","status":"Paused","text":"Waiting","future":{"nested":[1]}}"#,
                resource("x", ResourceStatus::Other("Paused".to_owned()), "Waiting"),
            ),
            (
                r#"{"id":"x","text":"Waiting","percent":250,"current":-1,"total":"big","details":7}"#,
                resource("x", ResourceStatus::Working, "Waiting"),
            ),
            (
                r#"  {"id":"x","status":"Done"}  "#,
                resource("x", ResourceStatus::Done, ""),
            ),
        ];
        for (line, want) in cases {
            assert_eq!(
                parse_progress(line),
                Some(ProgressEvent::Resource(want)),
                "{line}"
            );
        }
    }

    #[test]
    fn parses_messages() {
        let cases = [
            (
                r#"{"error":true,"message":"Error response from daemon: pull access denied"}"#,
                true,
                "Error response from daemon: pull access denied",
            ),
            (r#"{"message":"hello"}"#, false, "hello"),
            (r#"{"error":"yes","message":"odd"}"#, false, "odd"),
            (
                r#"{"tail":true,"text":"Attaching to web"}"#,
                false,
                "Attaching to web",
            ),
        ];
        for (line, error, message) in cases {
            assert_eq!(
                parse_progress(line),
                Some(ProgressEvent::Message {
                    error,
                    message: message.to_owned()
                }),
                "{line}"
            );
        }
    }

    #[test]
    fn parses_buildkit_status() {
        let line = r#"{"vertexes":[{"digest":"sha256:be52","inputs":["sha256:5848"],"name":"[2/2] RUN echo hi","started":"2026-10-24T00:33:39.530592792Z"},{"digest":"sha256:5848","name":"[1/2] FROM alpine","started":"2026-10-24T00:33:39Z","completed":"2026-10-24T00:33:40Z","cached":true},{"digest":"sha256:dead","name":"RUN false","error":"exit code: 1"},{"name":"no digest"}]}"#;
        assert_eq!(
            parse_progress(line),
            Some(ProgressEvent::Build(BuildProgress {
                vertexes: vec![
                    BuildVertex {
                        digest: "sha256:be52".to_owned(),
                        name: "[2/2] RUN echo hi".to_owned(),
                        started: true,
                        completed: false,
                        cached: false,
                        error: None,
                    },
                    BuildVertex {
                        digest: "sha256:5848".to_owned(),
                        name: "[1/2] FROM alpine".to_owned(),
                        started: true,
                        completed: true,
                        cached: true,
                        error: None,
                    },
                    BuildVertex {
                        digest: "sha256:dead".to_owned(),
                        name: "RUN false".to_owned(),
                        started: false,
                        completed: false,
                        cached: false,
                        error: Some("exit code: 1".to_owned()),
                    },
                ],
                logs: Vec::new(),
            }))
        );
        for line in [
            r#"{"statuses":[{"id":"transferring context:","current":2}]}"#,
            r#"{"vertexes":"not a list"}"#,
            r#"{"logs":"not a list"}"#,
        ] {
            assert_eq!(
                parse_progress(line),
                Some(ProgressEvent::Build(BuildProgress::default())),
                "{line}"
            );
        }
    }

    #[test]
    fn parses_buildkit_logs() {
        let log = |vertex: &str, stream: u8, data: &[u8]| VertexLog {
            vertex: vertex.to_owned(),
            stream,
            data: data.to_vec(),
        };
        let cases = [
            (
                "recorded from a failing RUN",
                r#"{"vertexes":[{"digest":"sha256:9c2f","inputs":["sha256:5848"],"name":"[2/2] RUN echo building && exit 3","started":"2026-10-24T00:33:39.5Z"}],"logs":[{"vertex":"sha256:9c2f","stream":1,"data":"YnVpbGRpbmcK","timestamp":"2026-10-24T00:33:39.6Z"},{"vertex":"sha256:9c2f","stream":2,"data":"b29wcw==","timestamp":"2026-10-24T00:33:39.6Z"}]}"#,
                vec![
                    log("sha256:9c2f", 1, b"building\n"),
                    log("sha256:9c2f", 2, b"oops"),
                ],
            ),
            (
                "bytes that are not utf-8",
                r#"{"logs":[{"vertex":"sha256:a","stream":1,"data":"/w=="}]}"#,
                vec![log("sha256:a", 1, b"\xff")],
            ),
            (
                "a missing or odd stream is standard output",
                r#"{"logs":[{"vertex":"a","data":"aGkK"},{"vertex":"b","stream":"2","data":"aGkK"},{"vertex":"c","stream":300,"data":"aGkK"}]}"#,
                vec![
                    log("a", 1, b"hi\n"),
                    log("b", 1, b"hi\n"),
                    log("c", 1, b"hi\n"),
                ],
            ),
            (
                "entries that cannot be read are left out",
                r#"{"logs":[{"vertex":"a","stream":1,"data":"not base64!"},{"stream":1,"data":"aGkK"},{"vertex":"b","stream":1},{"vertex":"c","stream":1,"data":""},{"vertex":"d","stream":1,"data":7},"text",{"vertex":"e","stream":1,"data":"aGkK"}]}"#,
                vec![log("e", 1, b"hi\n")],
            ),
        ];
        for (name, line, want) in cases {
            let Some(ProgressEvent::Build(build)) = parse_progress(line) else {
                panic!("{name}: not a build line");
            };
            assert_eq!(build.logs, want, "{name}");
        }
    }

    #[test]
    fn ignores_everything_else() {
        for line in [
            "",
            "   ",
            "Container blog-web-1  Started",
            "no such service: web",
            "{",
            "{not json}",
            r#"{"id":"x"}"#,
            r#"{"id":"","status":"Done","text":"Started"}"#,
            r#"{"tail":true}"#,
            r#"{"tail":false,"text":"x"}"#,
            r#"{"unknown":1}"#,
            "{}",
            "[1,2]",
            r#""string""#,
            r#"{"id":"x","status":"Done"} trailing"#,
        ] {
            assert_eq!(parse_progress(line), None, "{line:?}");
        }
    }
}
