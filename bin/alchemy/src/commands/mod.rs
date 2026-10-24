//! One module per subcommand.
//!
//! Each module has a clap `Args` struct, a `run` that does the command's
//! work, and pure functions that render its output.

pub mod app;
pub mod completions;
pub mod generate_man;
pub mod machine;
#[cfg(feature = "test-command")]
pub mod safety_probe;
pub mod status;
mod usage;
pub mod version;

use clap::CommandFactory;

use crate::Cli;

/// The command tree users see: [`Cli`]'s, without hidden subcommands at
/// any depth. Completions and man pages are generated from it.
///
/// clap's generators list hidden subcommands, and clap cannot remove a
/// subcommand, so a command with hidden subcommands is rebuilt from its
/// parts. Tests check the rebuilt commands' help is unchanged.
pub(crate) fn visible_tree() -> clap::Command {
    prune(Cli::command())
}

fn prune(cmd: clap::Command) -> clap::Command {
    if !cmd.get_subcommands().any(clap::Command::is_hide_set) {
        return cmd.mut_subcommands(prune);
    }
    let subcommands: Vec<_> = cmd
        .get_subcommands()
        .filter(|sub| !sub.is_hide_set())
        .cloned()
        .map(prune)
        .collect();
    let visible_aliases: Vec<_> = cmd.get_visible_aliases().map(str::to_owned).collect();
    let hidden_aliases: Vec<_> = cmd
        .get_all_aliases()
        .filter(|alias| !visible_aliases.iter().any(|v| v == alias))
        .map(str::to_owned)
        .collect();
    let mut rebuilt = clap::Command::new(cmd.get_name().to_owned())
        .args(cmd.get_arguments().cloned())
        .groups(cmd.get_groups().cloned())
        .subcommands(subcommands)
        .aliases(hidden_aliases)
        .visible_aliases(visible_aliases)
        .subcommand_required(cmd.is_subcommand_required_set())
        .arg_required_else_help(cmd.is_arg_required_else_help_set())
        .propagate_version(cmd.is_propagate_version_set())
        .disable_help_subcommand(cmd.is_disable_help_subcommand_set())
        .disable_help_flag(cmd.is_disable_help_flag_set())
        .disable_version_flag(cmd.is_disable_version_flag_set());
    if let Some(version) = cmd.get_version() {
        rebuilt = rebuilt.version(version.to_owned());
    }
    if let Some(version) = cmd.get_long_version() {
        rebuilt = rebuilt.long_version(version.to_owned());
    }
    if let Some(author) = cmd.get_author() {
        rebuilt = rebuilt.author(author.to_owned());
    }
    if let Some(name) = cmd.get_subcommand_value_name() {
        rebuilt = rebuilt.subcommand_value_name(name.to_owned());
    }
    if let Some(heading) = cmd.get_subcommand_help_heading() {
        rebuilt = rebuilt.subcommand_help_heading(heading.to_owned());
    }
    if let Some(text) = cmd.get_about() {
        rebuilt = rebuilt.about(text.clone());
    }
    if let Some(text) = cmd.get_long_about() {
        rebuilt = rebuilt.long_about(text.clone());
    }
    if let Some(text) = cmd.get_before_help() {
        rebuilt = rebuilt.before_help(text.clone());
    }
    if let Some(text) = cmd.get_before_long_help() {
        rebuilt = rebuilt.before_long_help(text.clone());
    }
    if let Some(text) = cmd.get_after_help() {
        rebuilt = rebuilt.after_help(text.clone());
    }
    if let Some(text) = cmd.get_after_long_help() {
        rebuilt = rebuilt.after_long_help(text.clone());
    }
    rebuilt
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

    fn commands(path: &str, cmd: &clap::Command) -> Vec<(String, clap::Command)> {
        let path = format!("{path} {}", cmd.get_name()).trim().to_owned();
        let mut all = vec![(path.clone(), cmd.clone())];
        for sub in cmd.get_subcommands() {
            all.extend(commands(&path, sub));
        }
        all
    }

    fn built(mut cmd: clap::Command) -> clap::Command {
        cmd.build();
        cmd
    }

    #[test]
    fn the_visible_tree_has_no_hidden_commands() {
        let tree = built(visible_tree());
        for (path, cmd) in commands("", &tree) {
            assert!(!cmd.is_hide_set(), "{path}");
        }
        let names: Vec<_> = tree
            .get_subcommands()
            .map(clap::Command::get_name)
            .collect();
        assert!(names.contains(&"status"), "{names:?}");
        assert!(!names.contains(&"generate-man"), "{names:?}");
    }

    #[test]
    fn pruning_keeps_every_visible_commands_help() {
        let full = built(Cli::command());
        let tree = built(visible_tree());
        let pruned = commands("", &tree);
        for (path, mut want) in commands("", &full) {
            if want.is_hide_set() {
                continue;
            }
            let Some((_, got)) = pruned.iter().find(|(p, _)| *p == path) else {
                panic!("{path} missing");
            };
            let mut got = got.clone();
            assert_eq!(
                got.render_long_help().to_string(),
                want.render_long_help().to_string(),
                "{path}"
            );
            assert_eq!(
                got.render_usage().to_string(),
                want.render_usage().to_string(),
                "{path}"
            );
            assert_eq!(got.render_long_version(), want.render_long_version());
        }
    }
}
