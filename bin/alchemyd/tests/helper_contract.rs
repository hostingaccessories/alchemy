//! The contract between alchemyd and `alchemy-helper`: every argv
//! alchemyd builds, through every elevation, parses in the helper as the
//! operation it means. The helper depends on no internal crate, so this
//! is the only thing that keeps the two in step.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use alchemy_core::privilege::{
    Elevation, HelperOp, HelperPath, ProxyPorts, ServiceUid, TerminalReason, elevated_command,
};

fn ops() -> Vec<HelperOp> {
    let mut ops = vec![HelperOp::GrantLowPorts];
    for proxy_ports in [
        ProxyPorts::DEFAULT,
        ProxyPorts::new(1024, 1024).unwrap(),
        ProxyPorts::new(9000, 65535).unwrap(),
    ] {
        ops.push(HelperOp::OpenFirewall { proxy_ports });
    }
    for uid in [1, 501, 1000, u32::MAX] {
        ops.push(HelperOp::InstallService {
            uid: ServiceUid::new(uid).unwrap(),
        });
    }
    let undos: Vec<HelperOp> = ops.iter().map(HelperOp::undo).collect();
    ops.extend(undos);
    ops
}

#[test]
fn every_argv_alchemyd_builds_parses_in_the_helper() {
    let helpers = [
        "/usr/libexec/alchemy/alchemy-helper",
        "/Library/PrivilegedHelperTools/alchemy-helper",
        "/opt/Alchemy's Tools/alchemy-helper",
    ];
    let elevations = [
        Elevation::Pkexec,
        Elevation::Osascript,
        Elevation::Terminal {
            reason: TerminalReason::NoPolkitAgent,
        },
    ];
    for helper in helpers {
        let helper = HelperPath::new(helper).unwrap();
        for elevation in &elevations {
            for op in ops() {
                let command = elevated_command(elevation, &helper, &op);
                let at = command
                    .argv()
                    .iter()
                    .position(|arg| arg == helper.as_str())
                    .unwrap_or_else(|| panic!("no helper path in {command}"));
                let argv = &command.argv()[at..];
                assert_eq!(argv[1..], op.args(), "{command}");
                let parsed =
                    alchemy_helper::parse(argv).unwrap_or_else(|e| panic!("{command}: {e}"));
                assert_eq!(parsed.name(), op.subcommand(), "{command}");
            }
        }
    }
}

#[test]
fn the_helper_refuses_what_alchemyd_never_builds() {
    let helper = "/usr/libexec/alchemy/alchemy-helper";
    let bad: [&[&str]; 6] = [
        &["grant-low-ports", "/usr/bin/sh"],
        &["open-firewall", "--proxy-ports", "22-8999"],
        &["open-firewall", "--proxy-ports", "8100-8999\n"],
        &["install-service", "--uid", "0"],
        &["remove-service"],
        &["grant-low-ports", "--path", "/tmp/alchemyd"],
    ];
    for args in bad {
        let argv = std::iter::once(helper).chain(args.iter().copied());
        let error = alchemy_helper::parse(argv).unwrap_err();
        assert!(error.is_usage(), "{args:?}");
    }
}

/// `GET /api/machine/autostart` points at the log the helper's launch
/// daemon writes.
#[test]
fn the_boot_log_hint_is_where_the_launch_daemon_logs() {
    use alchemy_core::lifecycle::{
        AutostartMode, LogHint, SYSTEM_LOG_DIR, log_hint, system_log_path,
    };
    use alchemy_core::route::HostOs;
    assert_eq!(SYSTEM_LOG_DIR, alchemy_helper::LAUNCH_DAEMON_LOG_DIR);
    for uid in [1, 501, 1000, u32::MAX] {
        let helper = std::path::PathBuf::from(alchemy_helper::launch_daemon_log(uid));
        assert_eq!(system_log_path(uid), helper);
        assert_eq!(
            log_hint(
                HostOs::MacOs,
                AutostartMode::Boot,
                std::path::Path::new("/Users/jo"),
                uid
            ),
            LogHint::File { path: helper }
        );
    }
}
