//! App logs.
//!
//! `GET /api/apps/{name}/logs?service=&tail=&follow=` is a server-sent
//! event stream of [`LogEvent`]s:
//!
//! 1. one `log` event per line of the last `tail` lines (200 by default,
//!    at most 5000) of the app's containers, oldest first, ordered by the
//!    engine's timestamps across containers. Only as many of the newest
//!    of those as fit in 2 MiB are sent;
//! 2. with `follow=true`, one `following` event, then a `log` event for
//!    each new line as it arrives, until the client goes away;
//! 3. without it, one `done` event, which ends the stream. A `done` with
//!    an `error` ends a followed stream too.
//!
//! A stream that ends without `done` was cut off, such as when alchemyd
//! stops. `service` limits the lines to one of the app's services; an
//! unknown app is a 404 with
//! [`ErrorCode::AppNotFound`](crate::ErrorCode::AppNotFound), and an
//! unknown service or a bad `tail` or `follow` a 400 with
//! [`ErrorCode::BadRequest`](crate::ErrorCode::BadRequest), before any
//! event is sent. While alchemyd already serves 16 log streams, followed
//! or not, a new request is a 503 with
//! [`ErrorCode::Busy`](crate::ErrorCode::Busy).

use serde::{Deserialize, Serialize, Serializer};

/// One line of a container's log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogLine {
    /// The Compose service the container runs.
    pub service: String,
    /// The container's name.
    pub container: String,
    /// Which output the line was written to.
    pub stream: LogStream,
    /// When the engine received the line, as RFC 3339 in UTC with up to
    /// nine fractional digits, or `None` when it did not say.
    pub time: Option<String>,
    /// The line, without its newline. Bytes that are not UTF-8 are
    /// replaced with U+FFFD, and a line over 8 KiB is cut and ends in `…`.
    pub line: String,
}

/// The output a [`LogLine`] was written to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum LogStream {
    /// Standard output, and everything a container with a terminal
    /// writes.
    Stdout,
    /// Standard error.
    Stderr,
    /// An output this build does not know.
    #[serde(other)]
    Other,
}

/// The `following` event: the tail has been sent and new lines follow.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsFollowing {}

/// The `done` event: the stream is complete.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogsDone {
    /// Why the logs could not be read in full, safe to show as is, or
    /// `None` when every line asked for was sent.
    pub error: Option<String>,
}

/// One message on a logs stream.
///
/// Sent as an SSE frame whose `event:` field is [`LogEvent::name`] and
/// whose `data:` field is the payload as JSON, as with
/// [`ApiEvent`](crate::ApiEvent).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum LogEvent {
    /// One line.
    Log(LogLine),
    /// The tail has been sent and new lines follow.
    Following(LogsFollowing),
    /// The stream is complete.
    Done(LogsDone),
}

impl LogEvent {
    /// The SSE `event:` name for this variant.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Log(_) => "log",
            Self::Following(_) => "following",
            Self::Done(_) => "done",
        }
    }

    /// The payload alone, which serializes as the SSE `data:` field.
    pub fn data(&self) -> LogEventData<'_> {
        LogEventData(self)
    }
}

/// A [`LogEvent`]'s payload without its name, from [`LogEvent::data`].
#[derive(Debug, Clone, Copy)]
pub struct LogEventData<'a>(&'a LogEvent);

impl Serialize for LogEventData<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            LogEvent::Log(line) => line.serialize(serializer),
            LogEvent::Following(following) => following.serialize(serializer),
            LogEvent::Done(done) => done.serialize(serializer),
        }
    }
}
