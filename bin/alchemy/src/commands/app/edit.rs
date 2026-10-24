//! `alchemy app edit NAME`: change an app's compose file or `.env` in
//! your own editor, validated by Compose before it is saved.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use alchemy_api::ErrorCode;
use alchemy_api::apps::{AppFile, AppFileUpdate};

use super::path_segment;
use crate::context::{Context, Sent};
use crate::daemon::{Daemon, Method};
use crate::editor::Editor;
use crate::error::ClientError;
use crate::prompt::Prompt;
use crate::safety::{self, Mode, Safety};

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app edit";

/// How every note Alchemy adds to the file being edited starts. Leading
/// lines that start with it are removed before the file is sent.
pub(crate) const NOTE: &str = "# alchemy:";

/// Arguments to `alchemy app edit`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// The app's name: its folder's name.
    pub name: String,

    /// Edit the app's .env instead of its compose file.
    #[arg(long)]
    pub env: bool,

    /// The shared safety flags. --force saves over changes made to the
    /// file on disk since it was opened.
    #[command(flatten)]
    pub safety: Safety,
}

/// What became of one attempt to save the edited text.
enum Attempt {
    Saved(AppFile),
    Rejected(String),
    Conflict(String),
}

/// Run `alchemy app edit`: open a private copy of the file in the editor,
/// have alchemyd validate the result with Compose, re-open it with
/// Compose's message on top until it passes or is left unchanged, confirm,
/// then save it. With `--dry-run`, stop once it passes.
pub(crate) async fn run<D: Daemon, W: Write, P: Prompt, E: Editor>(
    ctx: &mut Context<D, W>,
    prompt: &mut P,
    editor: &mut E,
    args: Args,
) -> Result<(), ClientError> {
    if !prompt.is_interactive() {
        return Err(ClientError::Usage {
            message: "`app edit` opens an editor, so it needs a terminal".to_owned(),
            hint: Some("to change the file from a script, edit it in the app's folder".to_owned()),
        });
    }
    let kind = if args.env { "env" } else { "compose" };
    let path = format!("/api/apps/{}/files/{kind}", path_segment(&args.name));
    let file = ctx.get::<AppFile>(&path).await?;
    let scratch = Scratch::new(&file.name)?;
    let mut base = file.hash.clone();
    let mut draft = file.content.clone();
    let mut note: Option<String> = None;
    loop {
        scratch.write(&annotate(&draft, note.as_deref()))?;
        editor.edit(scratch.path()).await?;
        let read = scratch.read()?;
        let edited = if note.is_some() {
            strip_notes(&read)
        } else {
            read
        };
        if edited == draft {
            return match note {
                None => {
                    let line = unchanged_line(&args.name, &file.name, ctx.json());
                    ctx.out().line(line)
                }
                Some(message) => Err(ClientError::Daemon(format!(
                    "{} was not saved: {message}",
                    file.name
                ))),
            };
        }
        draft = edited;
        let checked = attempt(ctx, &path, &draft, &mut base, true, args.safety.force).await?;
        let saved = match checked {
            Attempt::Saved(_) => {
                if args.safety.dry_run {
                    let line = valid_line(&args.name, &file.name, ctx.json());
                    return ctx.out().line(line);
                }
                let question = format!("Save {} for {}?", file.name, args.name);
                if safety::confirm(prompt, &args.safety, NAME, &question).await? == Mode::DryRun {
                    return Ok(());
                }
                attempt(ctx, &path, &draft, &mut base, false, args.safety.force).await?
            }
            other => other,
        };
        match saved {
            Attempt::Saved(saved) => {
                let line = saved_line(&args.name, &saved, ctx.json());
                return ctx.out().line(line);
            }
            Attempt::Rejected(message) => note = Some(message),
            Attempt::Conflict(message) => {
                let kept = scratch.keep();
                return Err(ClientError::Daemon(format!(
                    "{message} Your edit is in {}; pass --force to save it anyway.",
                    kept.display()
                )));
            }
        }
    }
}

/// Send `content` to be validated, or saved unless `dry_run`, against
/// `base`. With `force`, a conflict takes the file's current hash as the
/// new base and tries once more.
async fn attempt<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    path: &str,
    content: &str,
    base: &mut Option<String>,
    dry_run: bool,
    force: bool,
) -> Result<Attempt, ClientError> {
    let mut forced = false;
    loop {
        let update = AppFileUpdate {
            content: content.to_owned(),
            base_hash: base.clone(),
            dry_run,
        };
        match ctx.send::<_, AppFile>(Method::PUT, path, &update).await? {
            Sent::Done(file) => return Ok(Attempt::Saved(file)),
            Sent::Refused(refusal) if refusal.code == ErrorCode::Invalid => {
                return Ok(Attempt::Rejected(refusal.error));
            }
            Sent::Refused(_) if force && !forced => {
                *base = ctx.get::<AppFile>(path).await?.hash;
                forced = true;
            }
            Sent::Refused(refusal) => return Ok(Attempt::Conflict(refusal.error)),
        }
    }
}

/// `content` with `error`, if any, on top as notes saying how to go on.
pub(crate) fn annotate(content: &str, error: Option<&str>) -> String {
    let Some(error) = error else {
        return content.to_owned();
    };
    let mut lines = vec![format!("{NOTE} Compose rejected this file:")];
    lines.extend(error.lines().map(|line| format!("{NOTE}   {line}")));
    lines.push(format!(
        "{NOTE} Fix it and save to try again, or quit without changing it to give up."
    ));
    lines.push(format!(
        "{NOTE} Lines starting with \"{NOTE}\" are removed."
    ));
    let mut text = lines.join("\n");
    text.push('\n');
    text.push_str(content);
    text
}

/// `text` without the notes [`annotate`] put on top. Only called on text
/// that was annotated, so a file of the person's own that starts with
/// [`NOTE`] is left alone.
pub(crate) fn strip_notes(text: &str) -> String {
    let mut rest = text;
    while rest.starts_with(NOTE) {
        rest = rest.split_once('\n').map_or("", |(_, after)| after);
    }
    rest.to_owned()
}

fn unchanged_line(app: &str, file: &str, json: bool) -> String {
    if json {
        serde_json::json!({ "app": app, "file": file, "saved": false, "dry_run": false })
            .to_string()
    } else {
        format!("{file} for {app} is unchanged; nothing was saved")
    }
}

fn valid_line(app: &str, file: &str, json: bool) -> String {
    if json {
        serde_json::json!({ "app": app, "file": file, "saved": false, "dry_run": true }).to_string()
    } else {
        format!("would save {file} for {app}: Compose accepts it")
    }
}

fn saved_line(app: &str, file: &AppFile, json: bool) -> String {
    if json {
        serde_json::json!({
            "app": app,
            "file": file.name,
            "saved": true,
            "dry_run": false,
            "hash": file.hash,
        })
        .to_string()
    } else {
        format!("saved {} for {app}", file.name)
    }
}

/// A private copy of the file to edit, in a folder only its owner can
/// open, removed when dropped unless kept.
struct Scratch {
    dir: Option<tempfile::TempDir>,
    path: PathBuf,
}

impl Scratch {
    fn new(file_name: &str) -> Result<Self, ClientError> {
        let failed = |err: std::io::Error| {
            ClientError::File(format!("cannot make a private copy to edit: {err}"))
        };
        let dir = tempfile::Builder::new()
            .prefix("alchemy-edit-")
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .map_err(failed)?;
        let path = dir.path().join(file_name);
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(failed)?;
        Ok(Self {
            dir: Some(dir),
            path,
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, text: &str) -> Result<(), ClientError> {
        fs::write(&self.path, text).map_err(|err| {
            ClientError::File(format!("cannot write {}: {err}", self.path.display()))
        })
    }

    fn read(&self) -> Result<String, ClientError> {
        let bytes = fs::read(&self.path).map_err(|err| {
            ClientError::File(format!("cannot read {}: {err}", self.path.display()))
        })?;
        super::create::check_input(&self.path.display().to_string(), bytes)
    }

    /// Keep the copy on disk and return where it is.
    fn keep(mut self) -> PathBuf {
        if let Some(dir) = self.dir.take() {
            let _ = dir.keep();
        }
        self.path.clone()
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
    use alchemy_api::apps::AppFileKind;

    use super::*;
    use crate::daemon::fake::FakeDaemon;
    use crate::editor::FakeEditor;
    use crate::prompt::FakePrompt;

    const PATH: &str = "/api/apps/blog/files/compose";
    const OLD: &str = "services:\n  web:\n    image: nginx\n";

    fn file(content: &str, hash: &str) -> AppFile {
        AppFile {
            kind: AppFileKind::Compose,
            name: "compose.yaml".into(),
            content: content.into(),
            hash: Some(hash.into()),
        }
    }

    fn args(dry_run: bool, yes: bool, force: bool) -> Args {
        Args {
            name: "blog".into(),
            env: false,
            safety: Safety {
                dry_run,
                force,
                yes,
            },
        }
    }

    fn base(body: &str) -> serde_json::Value {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        serde_json::json!([body["base_hash"], body["dry_run"]])
    }

    #[test]
    fn notes_go_on_top_and_come_off_again() {
        let cases = [
            (OLD, None),
            (OLD, Some("yaml: line 2: bad")),
            ("", Some("one\ntwo")),
            ("a: 1\n# alchemy: mine, below other text\n", Some("bad")),
        ];
        for (content, error) in cases {
            let annotated = annotate(content, error);
            assert_eq!(annotated.starts_with(NOTE), error.is_some(), "{content:?}");
            if let Some(error) = error {
                for line in error.lines() {
                    assert!(annotated.contains(line), "{annotated}");
                }
            }
            assert_eq!(strip_notes(&annotated), content, "{content:?} {error:?}");
        }
        assert_eq!(strip_notes("# alchemy: x"), "");
        assert_eq!(strip_notes("a\n# alchemy: x\n"), "a\n# alchemy: x\n");
    }

    #[tokio::test]
    async fn validates_confirms_then_saves() {
        let new = "services:\n  web:\n    image: caddy\n";
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &file(OLD, "h1"))
            .ok(Method::PUT, PATH, &file(new, "h2"))
            .ok(Method::PUT, PATH, &file(new, "h2"));
        let mut ctx = Context::buffered(fake, false, NAME);
        let mut prompt = FakePrompt::tty([true]);
        let mut editor = FakeEditor::saving([new]);
        run(
            &mut ctx,
            &mut prompt,
            &mut editor,
            args(false, false, false),
        )
        .await
        .unwrap();
        assert_eq!(ctx.written(), "saved compose.yaml for blog\n");
        assert_eq!(prompt.asked(), ["Save compose.yaml for blog?"]);
        assert_eq!(editor.seen(), [OLD]);
        let bodies = ctx.daemon().bodies();
        assert_eq!(base(&bodies[0]), serde_json::json!(["h1", true]));
        assert_eq!(base(&bodies[1]), serde_json::json!(["h1", false]));
    }

    #[tokio::test]
    async fn reopens_with_composes_message_until_it_passes() {
        let bad = "services:\n  web:\n    imagee: caddy\n";
        let good = "services:\n  web:\n    image: caddy\n";
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &file(OLD, "h1"))
            .error(
                Method::PUT,
                PATH,
                422,
                "invalid",
                "services.web imagee is not allowed",
            )
            .ok(Method::PUT, PATH, &file(good, "h2"));
        let mut ctx = Context::buffered(fake, false, NAME);
        let mut editor = FakeEditor::saving([bad, good]);
        run(
            &mut ctx,
            &mut FakePrompt::tty([]),
            &mut editor,
            args(true, false, false),
        )
        .await
        .unwrap();
        assert_eq!(
            ctx.written(),
            "would save compose.yaml for blog: Compose accepts it\n"
        );
        assert_eq!(editor.seen()[0], OLD);
        assert!(editor.seen()[1].contains("services.web imagee is not allowed"));
        assert_eq!(strip_notes(&editor.seen()[1]), bad);
    }

    #[tokio::test]
    async fn gives_up_when_left_unchanged() {
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &file(OLD, "h1"));
        let mut ctx = Context::buffered(fake, false, NAME);
        run(
            &mut ctx,
            &mut FakePrompt::tty([]),
            &mut FakeEditor::saving([]),
            args(false, true, false),
        )
        .await
        .unwrap();
        assert_eq!(
            ctx.written(),
            "compose.yaml for blog is unchanged; nothing was saved\n"
        );

        let bad = "services: [\n";
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &file(OLD, "h1"))
            .error(Method::PUT, PATH, 422, "invalid", "yaml: bad");
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(
            &mut ctx,
            &mut FakePrompt::tty([]),
            &mut FakeEditor::saving([bad]),
            args(false, true, false),
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "alchemyd: compose.yaml was not saved: yaml: bad"
        );
    }

    #[tokio::test]
    async fn a_conflict_keeps_the_edit_unless_forced() {
        let new = "services: {}\n";
        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &file(OLD, "h1"))
            .error(
                Method::PUT,
                PATH,
                409,
                "conflict",
                "compose.yaml changed on disk.",
            );
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(
            &mut ctx,
            &mut FakePrompt::tty([]),
            &mut FakeEditor::saving([new]),
            args(false, true, false),
        )
        .await
        .unwrap_err();
        let message = err.to_string();
        let kept = message
            .split_once("Your edit is in ")
            .and_then(|(_, rest)| rest.split_once(';'))
            .map(|(path, _)| PathBuf::from(path))
            .unwrap();
        assert_eq!(fs::read_to_string(&kept).unwrap(), new);
        assert_eq!(
            fs::metadata(&kept).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(kept.parent().unwrap()).unwrap();

        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &file(OLD, "h1"))
            .error(Method::PUT, PATH, 409, "conflict", "changed")
            .ok(Method::GET, PATH, &file("by hand", "h9"))
            .ok(Method::PUT, PATH, &file(new, "h2"))
            .ok(Method::PUT, PATH, &file(new, "h2"));
        let mut ctx = Context::buffered(fake, false, NAME);
        run(
            &mut ctx,
            &mut FakePrompt::tty([]),
            &mut FakeEditor::saving([new]),
            args(false, true, true),
        )
        .await
        .unwrap();
        let bodies = ctx.daemon().bodies();
        assert_eq!(base(&bodies[1]), serde_json::json!(["h9", true]));
        assert_eq!(base(&bodies[2]), serde_json::json!(["h9", false]));
    }

    #[tokio::test]
    async fn needs_a_terminal_and_a_yes() {
        let mut ctx = Context::buffered(FakeDaemon::new(), false, NAME);
        let err = run(
            &mut ctx,
            &mut FakePrompt::no_tty(),
            &mut FakeEditor::saving([]),
            args(false, true, false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(ctx.daemon().requests(), [] as [&str; 0]);

        let fake = FakeDaemon::new()
            .version("1.2.3", 1)
            .ok(Method::GET, PATH, &file(OLD, "h1"))
            .ok(Method::PUT, PATH, &file("x", "h2"));
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(
            &mut ctx,
            &mut FakePrompt::tty([false]),
            &mut FakeEditor::saving(["x"]),
            args(false, false, false),
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert_eq!(ctx.daemon().bodies().len(), 1);
    }
}
