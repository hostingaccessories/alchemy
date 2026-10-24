//! `alchemy app create NAME`: a new app from a compose file.

use std::io::{Read as _, Write};
use std::path::{Path, PathBuf};

use alchemy_api::apps::files::MAX_FILE_LEN;
use alchemy_api::apps::{App, NewApp, Saved};

use super::action::follow_start;
use super::parse_name;
use crate::context::{Context, Sent};
use crate::daemon::{Daemon, Method};
use crate::error::ClientError;
use crate::prompt::Prompt;
use crate::safety::{self, Mode, Safety};

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app create";

/// The compose file read when `--file` is not given.
const DEFAULT_FILE: &str = "compose.yaml";

/// Arguments to `alchemy app create`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The new app's name, which becomes its folder's name: lowercase
    /// letters, digits, - and _.
    pub name: String,

    /// The compose file to copy in, or - to read it from standard input.
    #[arg(long, short = 'f', value_name = "PATH", default_value = DEFAULT_FILE)]
    pub file: PathBuf,

    /// An .env file to copy in beside it. The copy is readable by you
    /// alone.
    #[arg(long, value_name = "PATH")]
    pub env_file: Option<PathBuf>,

    /// The app folder to create it in, as a full path. The home folder by
    /// default.
    #[arg(long, value_name = "PATH")]
    pub folder: Option<String>,

    /// Start the app once it is created, and follow it to the end as
    /// `alchemy app up` does.
    #[arg(long, conflicts_with = "dry_run")]
    pub start: bool,

    /// The shared safety flags.
    #[command(flatten)]
    pub safety: Safety,
}

/// Run `alchemy app create`: read the files, confirm, then have alchemyd
/// validate them with Compose and create the app, and with `--start` start
/// it, or with `--dry-run` only validate them.
pub(crate) async fn run<D: Daemon, W: Write, P: Prompt>(
    ctx: &mut Context<D, W>,
    prompt: &mut P,
    args: Args,
) -> Result<(), ClientError> {
    let name = parse_name(&args.name).map_err(|message| ClientError::Usage {
        message,
        hint: None,
    })?;
    let compose = read_input(&args.file)?;
    let env = args.env_file.as_deref().map(read_input).transpose()?;
    let question = format!("Create the app {name}?");
    let mode = safety::confirm(prompt, &args.safety, NAME, &question).await?;
    let dry_run = mode == Mode::DryRun;
    let new = NewApp {
        name: name.to_string(),
        folder: args.folder,
        compose,
        env,
        dry_run,
        start: args.start,
    };
    match ctx
        .send::<_, Saved<App>>(Method::POST, "/api/apps", &new)
        .await?
    {
        Sent::Done(saved) => {
            for line in created_lines(&saved.saved, dry_run, ctx.json()) {
                ctx.out().line(line)?;
            }
            follow_start(ctx, &saved, name.as_str(), args.start).await
        }
        Sent::Refused(refusal) => Err(ClientError::Daemon(refusal.error)),
    }
}

/// The text of the file at `path`, or of standard input for `-`, refused
/// if alchemyd could not take it.
fn read_input(path: &Path) -> Result<String, ClientError> {
    let shown = if path == Path::new("-") {
        "standard input".to_owned()
    } else {
        path.display().to_string()
    };
    let mut bytes = Vec::new();
    let limit = u64::try_from(MAX_FILE_LEN).unwrap_or(u64::MAX) + 1;
    let read = if path == Path::new("-") {
        std::io::stdin().take(limit).read_to_end(&mut bytes)
    } else {
        std::fs::File::open(path).and_then(|file| file.take(limit).read_to_end(&mut bytes))
    };
    read.map_err(|err| ClientError::File(format!("cannot read {shown}: {err}")))?;
    check_input(&shown, bytes)
}

/// `bytes` read from `shown` as text, if it is UTF-8 and small enough.
pub(crate) fn check_input(shown: &str, bytes: Vec<u8>) -> Result<String, ClientError> {
    if bytes.len() > MAX_FILE_LEN {
        return Err(ClientError::File(format!(
            "{shown} is larger than {} KiB",
            MAX_FILE_LEN / 1024
        )));
    }
    String::from_utf8(bytes).map_err(|_| ClientError::File(format!("{shown} isn't UTF-8 text")))
}

/// What `alchemy app create` prints: where the app went and what Compose
/// found in it. With `json`, `{"dry_run","app"}` on one line.
pub(crate) fn created_lines(app: &App, dry_run: bool, json: bool) -> Vec<String> {
    if json {
        return vec![serde_json::json!({ "dry_run": dry_run, "app": app }).to_string()];
    }
    let verb = if dry_run { "would create" } else { "created" };
    let mut lines = vec![format!("{verb} {} in {}", app.name, app.dir)];
    let services: Vec<&str> = app.services.iter().map(|s| s.name.as_str()).collect();
    if let Some(project) = &app.project {
        lines.push(if services.is_empty() {
            format!("Compose project {project}, with no services")
        } else {
            format!("Compose project {project}: {}", services.join(", "))
        });
    }
    lines
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]
mod tests {
    use alchemy_api::apps::{AppService, StackName, StackStatus, StopReason};
    use alchemy_api::operations::{Action, Operation, OperationEvent, OperationState};
    use alchemy_api::{ErrorCode, ErrorResponse};
    use clap::Parser as _;

    use super::*;
    use crate::Cli;
    use crate::daemon::fake::{FakeDaemon, sse};
    use crate::prompt::FakePrompt;

    fn app(services: &[&str]) -> App {
        App {
            name: StackName::new("blog").unwrap(),
            status: StackStatus::Unknown,
            dir: "/home/me/Alchemy/apps/blog".into(),
            root: "/home/me/Alchemy/apps".into(),
            compose_file: "compose.yaml".into(),
            project: Some("blog".into()),
            images: None,
            services: services
                .iter()
                .map(|s| AppService {
                    name: (*s).into(),
                    containers: vec![],
                })
                .collect(),
            problems: vec![],
            stop_reason: StopReason::default(),
            addresses: vec![],
        }
    }

    #[test]
    fn lines_say_where_and_what() {
        let cases: [(App, bool, &[&str]); 3] = [
            (
                app(&["web", "db"]),
                false,
                &[
                    "created blog in /home/me/Alchemy/apps/blog",
                    "Compose project blog: web, db",
                ],
            ),
            (
                app(&[]),
                true,
                &[
                    "would create blog in /home/me/Alchemy/apps/blog",
                    "Compose project blog, with no services",
                ],
            ),
            (
                App {
                    project: None,
                    ..app(&[])
                },
                false,
                &["created blog in /home/me/Alchemy/apps/blog"],
            ),
        ];
        for (app, dry_run, want) in cases {
            assert_eq!(created_lines(&app, dry_run, false), want);
        }
        let json: serde_json::Value =
            serde_json::from_str(&created_lines(&app(&[]), true, true)[0]).unwrap();
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["app"]["name"], "blog");
    }

    #[test]
    fn checks_input_like_alchemyd() {
        assert_eq!(check_input("x", b"a: 1\n".to_vec()).unwrap(), "a: 1\n");
        let cases = [
            (vec![0xff], "x isn't UTF-8 text"),
            (vec![b'a'; MAX_FILE_LEN + 1], "x is larger than 256 KiB"),
        ];
        for (bytes, want) in cases {
            assert_eq!(check_input("x", bytes).unwrap_err().to_string(), want);
        }
    }

    fn files() -> (tempfile::TempDir, Args) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").unwrap();
        std::fs::write(dir.path().join(".env"), "A=1\n").unwrap();
        let args = Args {
            name: "blog".into(),
            file: dir.path().join("compose.yaml"),
            env_file: Some(dir.path().join(".env")),
            folder: None,
            start: false,
            safety: Safety {
                yes: true,
                ..Safety::default()
            },
        };
        (dir, args)
    }

    #[tokio::test]
    async fn sends_the_files_and_prints_the_app() {
        let (_dir, args) = files();
        let fake = FakeDaemon::new().version("1.2.3", 1).reply(
            Method::POST,
            "/api/apps",
            201,
            serde_json::to_vec(&app(&["web"])).unwrap(),
        );
        let mut ctx = Context::buffered(fake, false, NAME);
        run(&mut ctx, &mut FakePrompt::no_tty(), args)
            .await
            .unwrap();
        assert_eq!(
            ctx.written(),
            "created blog in /home/me/Alchemy/apps/blog\nCompose project blog: web\n"
        );
        let body: serde_json::Value = serde_json::from_str(&ctx.daemon().bodies()[0]).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "name": "blog",
                "folder": null,
                "compose": "services: {}\n",
                "env": "A=1\n",
                "dry_run": false,
            })
        );
    }

    #[tokio::test]
    async fn a_dry_run_asks_alchemyd_to_validate_only() {
        let (_dir, mut args) = files();
        args.safety = Safety {
            dry_run: true,
            ..Safety::default()
        };
        args.folder = Some("/opt/stacks".into());
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::POST, "/api/apps", &app(&[]));
        let mut ctx = Context::buffered(fake, false, NAME);
        run(&mut ctx, &mut FakePrompt::no_tty(), args)
            .await
            .unwrap();
        assert!(ctx.written().starts_with("would create blog"));
        let body: serde_json::Value = serde_json::from_str(&ctx.daemon().bodies()[0]).unwrap();
        assert_eq!(body["dry_run"], true);
        assert_eq!(body["folder"], "/opt/stacks");
    }

    fn up(state: OperationState) -> Operation {
        Operation {
            id: "op1".into(),
            app: StackName::new("blog").unwrap(),
            action: Action::Up,
            state,
            steps: vec![],
            started_at: 0,
            finished_at: None,
            error: None,
        }
    }

    fn saved(operation: Option<Operation>, start_error: Option<ErrorResponse>) -> Vec<u8> {
        serde_json::to_vec(&Saved {
            saved: app(&["web"]),
            operation,
            start_error,
        })
        .unwrap()
    }

    #[tokio::test]
    async fn starts_the_app_and_follows_it_to_the_end() {
        let done = OperationEvent::Done(up(OperationState::Succeeded));
        let done = sse(done.name(), &serde_json::to_string(&done.data()).unwrap());
        let cases = [
            (
                false,
                "created blog in /home/me/Alchemy/apps/blog
Compose project blog: web
blog is up
",
            ),
            (true, ""),
        ];
        for (json, want) in cases {
            let (_dir, mut args) = files();
            args.start = true;
            let fake = FakeDaemon::new()
                .version("1.2.3", 1)
                .reply(
                    Method::POST,
                    "/api/apps",
                    201,
                    saved(Some(up(OperationState::Running)), None),
                )
                .stream("/api/operations/op1/events", [Ok(done.clone())]);
            let mut ctx = Context::buffered(fake, json, NAME);
            run(&mut ctx, &mut FakePrompt::no_tty(), args)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_str(&ctx.daemon().bodies()[0]).unwrap();
            assert_eq!(body["start"], true, "json {json}");
            if json {
                let lines: Vec<serde_json::Value> = ctx
                    .written()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                assert_eq!(lines.len(), 2, "{}", ctx.written());
                assert_eq!(lines[0]["app"]["name"], "blog");
                assert_eq!(lines[1]["event"], "done");
            } else {
                assert_eq!(ctx.written(), want);
                assert!(
                    ctx.progress_written()
                        .starts_with("Bringing up blog (operation op1).")
                );
            }
        }
    }

    #[tokio::test]
    async fn a_start_refused_after_the_save_fails_once_it_is_said() {
        let busy = "blog is busy with another operation (op9); wait for it to finish";
        let cases = [
            (
                saved(None, Some(ErrorResponse::new(ErrorCode::Conflict, busy))),
                format!("alchemyd: {busy}"),
            ),
            (
                saved(None, None),
                "alchemyd: saved blog, but this alchemyd doesn't start apps on save; run `alchemy app up blog`".to_owned(),
            ),
        ];
        for (reply, want) in cases {
            let (_dir, mut args) = files();
            args.start = true;
            let fake =
                FakeDaemon::new()
                    .version("1.2.3", 1)
                    .reply(Method::POST, "/api/apps", 201, reply);
            let mut ctx = Context::buffered(fake, false, NAME);
            let err = run(&mut ctx, &mut FakePrompt::no_tty(), args)
                .await
                .unwrap_err();
            assert_eq!(err.to_string(), want);
            assert_eq!(err.exit_code(), 1);
            assert!(ctx.written().starts_with("created blog in "), "{want}");
            assert_eq!(
                ctx.daemon().requests(),
                ["GET /api/version", "POST /api/apps"]
            );
        }
    }

    #[test]
    fn start_and_dry_run_are_refused_together() {
        let err = Cli::try_parse_from(["alchemy", "app", "create", "blog", "--start", "--dry-run"])
            .unwrap_err();
        assert!(err.to_string().contains("cannot be used with"), "{err}");
        Cli::try_parse_from(["alchemy", "app", "create", "blog", "--start", "--yes"]).unwrap();
    }

    #[tokio::test]
    async fn refusals_and_bad_input_fail_without_creating() {
        let (dir, args) = files();
        let fake = FakeDaemon::new().version("1.2.3", 1).error(
            Method::POST,
            "/api/apps",
            422,
            "invalid",
            "services.web Additional property imagee is not allowed",
        );
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(&mut ctx, &mut FakePrompt::no_tty(), args.clone())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "alchemyd: services.web Additional property imagee is not allowed"
        );
        assert_eq!(err.exit_code(), 1);

        let cases: [(Args, FakePrompt, u8); 4] = [
            (
                Args {
                    name: "My App".into(),
                    ..args.clone()
                },
                FakePrompt::no_tty(),
                2,
            ),
            (
                Args {
                    file: dir.path().join("missing.yaml"),
                    ..args.clone()
                },
                FakePrompt::no_tty(),
                1,
            ),
            (
                Args {
                    safety: Safety::default(),
                    ..args.clone()
                },
                FakePrompt::no_tty(),
                3,
            ),
            (
                Args {
                    safety: Safety::default(),
                    ..args.clone()
                },
                FakePrompt::tty([false]),
                3,
            ),
        ];
        for (args, mut prompt, want) in cases {
            let mut ctx = Context::buffered(FakeDaemon::new(), false, NAME);
            let err = run(&mut ctx, &mut prompt, args.clone()).await.unwrap_err();
            assert_eq!(err.exit_code(), want, "{args:?}: {err}");
            assert_eq!(ctx.daemon().requests(), [] as [&str; 0], "{args:?}");
        }
    }
}
