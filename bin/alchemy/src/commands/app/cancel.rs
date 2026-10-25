//! `alchemy app cancel NAME|OPERATION_ID`: stop a running operation.
//!
//! Given an app's name, it cancels that app's running operation, the first
//! running one in `/api/operations?app=NAME`. Anything that is not an app
//! name, or a name with no operations at all, is taken as an operation id.
//! Names come first, so an app named like an id can still be cancelled by
//! name. alchemyd interrupts the running step as Ctrl-C would interrupt
//! `docker compose`, and the operation ends as cancelled; the command does
//! not wait for that.

use std::io::Write;

use alchemy_api::ErrorCode;
use alchemy_api::operations::{OperationList, OperationResponse, OperationState, StackName};

use super::path_segment;
use crate::context::{Context, Sent};
use crate::daemon::Daemon;
use crate::error::ClientError;

/// The command as typed after `alchemy`, for errors.
pub(crate) const NAME: &str = "app cancel";

/// Arguments to `alchemy app cancel`.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Args {
    /// An app's name, to cancel its running operation, or the id of an
    /// operation, as `alchemy app up` and the other actions print it.
    #[arg(value_name = "NAME|OPERATION_ID")]
    pub target: String,
}

/// Run `alchemy app cancel`.
pub(crate) async fn run<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    args: Args,
) -> Result<(), ClientError> {
    let id = match running_for_app(ctx, &args.target).await? {
        Some(id) => id,
        None => args.target.clone(),
    };
    let path = format!("/api/operations/{}/cancel", path_segment(&id));
    let response: OperationResponse = match ctx.post_empty(&path).await? {
        Sent::Done(response) => response,
        Sent::Refused(refusal) => {
            return Err(match refusal.code {
                ErrorCode::Conflict => ClientError::NothingToCancel(refusal.error),
                ErrorCode::OperationNotFound => {
                    ClientError::NothingToCancel(not_found(&args.target))
                }
                _ => ClientError::Daemon(refusal.error),
            });
        }
    };
    let line = if ctx.json() {
        serde_json::json!(response).to_string()
    } else {
        let op = &response.operation;
        format!("Cancelling {} {} (operation {})", op.action, op.app, op.id)
    };
    ctx.out().line(line)?;
    Ok(())
}

/// The id of the running operation of the app named `target`. `None` when
/// `target` is no app name, or names nothing with operations, so it may be
/// an operation id; an error when the app has operations but none running.
async fn running_for_app<D: Daemon, W: Write>(
    ctx: &mut Context<D, W>,
    target: &str,
) -> Result<Option<String>, ClientError> {
    if StackName::new(target).is_err() {
        return Ok(None);
    }
    let path = format!("/api/operations?app={}", path_segment(target));
    let OperationList { operations } = ctx.get(&path).await?;
    if let Some(running) = operations
        .iter()
        .find(|op| op.state == OperationState::Running)
    {
        return Ok(Some(running.id.clone()));
    }
    if operations.is_empty() {
        return Ok(None);
    }
    Err(ClientError::NothingToCancel(format!(
        "{target} has no operation running"
    )))
}

/// Why nothing matched `target`, in the terms it could have meant.
fn not_found(target: &str) -> String {
    if StackName::new(target).is_ok() {
        format!(
            "no operation is running for an app named {target}, and no operation has id {target}"
        )
    } else {
        format!("no operation has id {target}")
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
    use alchemy_api::operations::{Action, Operation};

    use super::*;
    use crate::daemon::Method;
    use crate::daemon::fake::FakeDaemon;

    fn op(id: &str, action: Action, state: OperationState) -> Operation {
        Operation {
            id: id.into(),
            app: StackName::new("blog").unwrap(),
            action,
            state,
            steps: vec![],
            started_at: 0,
            finished_at: None,
            error: None,
            reason: None,
        }
    }

    fn list(operations: Vec<Operation>) -> OperationList {
        OperationList { operations }
    }

    fn args(target: &str) -> Args {
        Args {
            target: target.into(),
        }
    }

    const BLOG_OPS: &str = "/api/operations?app=blog";

    fn daemon() -> FakeDaemon {
        FakeDaemon::new().version("1.2.3", 1)
    }

    fn running(fake: FakeDaemon, path: &str) -> FakeDaemon {
        fake.ok(
            Method::GET,
            path,
            &list(vec![
                op("b2", Action::Update, OperationState::Running),
                op("a1", Action::Up, OperationState::Succeeded),
            ]),
        )
    }

    fn cancelled(fake: FakeDaemon, path: &str) -> FakeDaemon {
        fake.ok(
            Method::POST,
            path,
            &OperationResponse {
                operation: op("b2", Action::Update, OperationState::Running),
            },
        )
    }

    #[tokio::test]
    async fn cancels_by_name_or_id() {
        let cases = [
            (
                "by name",
                cancelled(running(daemon(), BLOG_OPS), "/api/operations/b2/cancel"),
                "blog",
                vec![
                    "GET /api/version",
                    "GET /api/operations?app=blog",
                    "POST /api/operations/b2/cancel",
                ],
            ),
            (
                "an id that is also a valid name",
                cancelled(
                    daemon().ok(
                        Method::GET,
                        "/api/operations?app=0192f3a4b5c60001",
                        &list(vec![]),
                    ),
                    "/api/operations/0192f3a4b5c60001/cancel",
                ),
                "0192f3a4b5c60001",
                vec![
                    "GET /api/version",
                    "GET /api/operations?app=0192f3a4b5c60001",
                    "POST /api/operations/0192f3a4b5c60001/cancel",
                ],
            ),
            (
                "an id that is no name",
                cancelled(daemon(), "/api/operations/Op%2F1/cancel"),
                "Op/1",
                vec!["GET /api/version", "POST /api/operations/Op%2F1/cancel"],
            ),
        ];
        for (name, fake, target, requests) in cases {
            let mut ctx = Context::buffered(fake, false, NAME);
            run(&mut ctx, args(target)).await.unwrap();
            assert_eq!(
                ctx.written(),
                "Cancelling update blog (operation b2)\n",
                "{name}"
            );
            assert_eq!(ctx.daemon().requests(), requests, "{name}");
        }
    }

    #[tokio::test]
    async fn prints_the_operation_as_json() {
        let fake = cancelled(running(daemon(), BLOG_OPS), "/api/operations/b2/cancel");
        let mut ctx = Context::buffered(fake, true, NAME);
        run(&mut ctx, args("blog")).await.unwrap();
        let written = ctx.written();
        assert_eq!(written.lines().count(), 1, "{written}");
        let back: OperationResponse = serde_json::from_str(written.trim()).unwrap();
        assert_eq!(back.operation.id, "b2");
        assert_eq!(back.operation.action, Action::Update);
    }

    #[tokio::test]
    async fn says_when_there_is_nothing_to_cancel() {
        let cases = [
            (
                "the app has nothing running",
                daemon().ok(
                    Method::GET,
                    BLOG_OPS,
                    &list(vec![op("a1", Action::Up, OperationState::Succeeded)]),
                ),
                "blog",
                "nothing to cancel: blog has no operation running",
            ),
            (
                "it finished meanwhile",
                running(daemon(), BLOG_OPS).error(
                    Method::POST,
                    "/api/operations/b2/cancel",
                    409,
                    "conflict",
                    "operation b2 has already finished",
                ),
                "blog",
                "nothing to cancel: operation b2 has already finished",
            ),
            (
                "neither an app with operations nor an id",
                daemon()
                    .ok(Method::GET, "/api/operations?app=blgo", &list(vec![]))
                    .error(
                        Method::POST,
                        "/api/operations/blgo/cancel",
                        404,
                        "operation_not_found",
                        "no operation has id \"blgo\"",
                    ),
                "blgo",
                "nothing to cancel: no operation is running for an app named blgo, and no operation has id blgo",
            ),
            (
                "an unknown id",
                daemon().error(
                    Method::POST,
                    "/api/operations/X1/cancel",
                    404,
                    "operation_not_found",
                    "no operation has id \"X1\"",
                ),
                "X1",
                "nothing to cancel: no operation has id X1",
            ),
        ];
        for (name, fake, target, want) in cases {
            let mut ctx = Context::buffered(fake, false, NAME);
            let err = run(&mut ctx, args(target)).await.unwrap_err();
            assert_eq!(err.to_string(), want, "{name}");
            assert_eq!(err.kind(), "nothing_to_cancel", "{name}");
            assert_eq!(err.exit_code(), 1, "{name}");
            assert_eq!(ctx.written(), "", "{name}");
        }
    }

    #[tokio::test]
    async fn other_refusals_and_old_daemons_pass_through() {
        let fake = daemon().error(
            Method::POST,
            "/api/operations/X1/cancel",
            500,
            "internal",
            "something broke",
        );
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(&mut ctx, args("X1")).await.unwrap_err();
        assert_eq!(err.kind(), "daemon", "{err}");
        assert_eq!(err.to_string(), "alchemyd: something broke");

        let fake = FakeDaemon::new().version("0.1.0", 1).error(
            Method::POST,
            "/api/operations/X1/cancel",
            404,
            "not_found",
            "not found",
        );
        let mut ctx = Context::buffered(fake, false, NAME);
        let err = run(&mut ctx, args("X1")).await.unwrap_err();
        assert_eq!(err.kind(), "daemon_too_old", "{err}");
        assert_eq!(
            err.to_string(),
            "alchemyd 0.1.0 is too old for `app cancel`"
        );
    }
}
