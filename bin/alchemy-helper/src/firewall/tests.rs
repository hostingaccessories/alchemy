use std::fmt::Write as _;

use super::firewalld::{SERVICE, service};
use super::macos::{SOCKETFILTERFW, apps, global_state};
use super::ufw::{PROFILE, profile};
use super::*;
use crate::fake::{Call, FakeFile, FakeSystem};
use crate::system::CommandOutput;

const UFW: &str = "/usr/sbin/ufw";
const FWCMD: &str = "/usr/bin/firewall-cmd";
const HELPER: &str = "/opt/alchemy/bin/alchemy-helper";
const DAEMON: &str = "/opt/alchemy/bin/alchemyd";

const ADDED: &str = "Rule added\nRule added (v6)\n";
const SKIPPED: &str = "Skipping adding existing rule\nSkipping adding existing rule (v6)\n";
const DELETED: &str = "Rule deleted\nRule deleted (v6)\n";
const NOT_DELETED: &str =
    "Could not delete non-existent rule\nCould not delete non-existent rule (v6)\n";
const ACTIVE: &str = "Status: active\n\nTo                         Action      From\n";
const ON: &str = "Firewall is enabled. (State = 1)\n";
const OFF: &str = "Firewall is disabled. (State = 0)\n";

fn listing(entries: &[(&str, bool)]) -> String {
    let mut out = format!("Total number of apps = {} \n", entries.len());
    for (i, (path, allowed)) in entries.iter().enumerate() {
        let verb = if *allowed { "Allow" } else { "Block" };
        let _ = write!(
            out,
            "{} : {path} 
             ({verb} incoming connections)
",
            i + 1
        );
    }
    out
}

#[derive(Clone, Copy)]
enum Op {
    Open,
    Close,
}

struct Case {
    name: &'static str,
    op: Op,
    platform: Platform,
    files: Vec<(&'static str, FakeFile)>,
    outputs: Vec<(&'static str, Vec<String>, CommandOutput)>,
    fail: Option<Call>,
    want: Result<bool, &'static str>,
    commands: Vec<String>,
    after: Vec<(&'static str, Option<String>)>,
}

fn case(name: &'static str, op: Op) -> Case {
    Case {
        name,
        op,
        platform: Platform::Linux,
        files: vec![],
        outputs: vec![],
        fail: None,
        want: Ok(true),
        commands: vec![],
        after: vec![],
    }
}

fn exe() -> FakeFile {
    FakeFile::regular()
}

fn file(contents: &str) -> FakeFile {
    FakeFile::with_contents(contents)
}

fn output(code: i32, stdout: &str) -> CommandOutput {
    CommandOutput {
        code: Some(code),
        stdout: stdout.as_bytes().to_vec(),
        stderr: Vec::new(),
    }
}

fn out(
    program: &'static str,
    args: &str,
    code: i32,
    stdout: &str,
) -> (&'static str, Vec<String>, CommandOutput) {
    (
        program,
        args.split(' ').map(str::to_owned).collect(),
        output(code, stdout),
    )
}

fn cmds(lines: &[&str]) -> Vec<String> {
    lines.iter().map(|&l| l.to_owned()).collect()
}

fn ufw_active() -> Vec<(&'static str, Vec<String>, CommandOutput)> {
    vec![out(UFW, "status", 0, ACTIVE)]
}

fn fwd_running() -> Vec<(&'static str, Vec<String>, CommandOutput)> {
    vec![out(FWCMD, "--state", 0, "running\n")]
}

fn old_range() -> ProxyPorts {
    ProxyPorts::new(9000, 9099).unwrap()
}

/// Every command run, with the program shortened to its file name.
fn short_commands(sys: &FakeSystem) -> Vec<String> {
    sys.argvs()
        .into_iter()
        .map(|mut argv| {
            argv[0] = argv[0].rsplit('/').next().unwrap().to_owned();
            argv.join(" ")
        })
        .collect()
}

fn check(cases: Vec<Case>) {
    for c in cases {
        let mut sys = FakeSystem::new(HELPER);
        for (path, f) in c.files {
            sys = sys.with_file(path, f);
        }
        for (program, args, o) in &c.outputs {
            let argv: Vec<&str> = std::iter::once(*program)
                .chain(args.iter().map(String::as_str))
                .collect();
            let stdout = String::from_utf8(o.stdout.clone()).unwrap();
            sys = sys.then_responding(&argv, o.code.unwrap(), &stdout);
        }
        if let Some(call) = c.fail {
            sys = sys.failing(call);
        }
        let result = match c.op {
            Op::Open => open(&sys, c.platform, ProxyPorts::DEFAULT),
            Op::Close => close(&sys, c.platform, ProxyPorts::DEFAULT),
        };
        let got = result.map(|o| o.changed).map_err(|e| e.kind());
        assert_eq!(got, c.want, "{}", c.name);
        assert_eq!(short_commands(&sys), c.commands, "{}", c.name);
        for (path, want) in c.after {
            let got = sys
                .file(path)
                .map(|f| String::from_utf8(f.contents).unwrap());
            assert_eq!(got, want, "{}: {path}", c.name);
            if let Some(f) = sys.file(path).filter(|_| want.is_some()) {
                assert_eq!(f.mode, 0o644, "{}: {path}", c.name);
            }
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn open_on_linux() {
    let new_profile = profile(ProxyPorts::DEFAULT);
    let new_service = service(ProxyPorts::DEFAULT);
    let open = || case("", Op::Open);
    check(vec![
        Case {
            name: "no firewall installed",
            want: Err("no_firewall"),
            after: vec![(PROFILE, None), (SERVICE, None)],
            ..open()
        },
        Case {
            name: "ufw installed but disabled",
            files: vec![(UFW, exe())],
            outputs: vec![out(UFW, "status", 0, "Status: inactive\n")],
            want: Err("no_firewall"),
            commands: cmds(&["ufw status"]),
            after: vec![(PROFILE, None)],
            ..open()
        },
        Case {
            name: "ufw: write the profile and allow it",
            files: vec![(UFW, exe())],
            outputs: [ufw_active(), vec![out(UFW, "allow alchemy", 0, ADDED)]].concat(),
            commands: cmds(&["ufw status", "ufw allow alchemy"]),
            after: vec![(PROFILE, Some(new_profile.clone()))],
            ..open()
        },
        Case {
            name: "ufw: already open",
            files: vec![(UFW, exe()), (PROFILE, file(&new_profile))],
            outputs: [ufw_active(), vec![out(UFW, "allow alchemy", 0, SKIPPED)]].concat(),
            want: Ok(false),
            commands: cmds(&["ufw status", "ufw allow alchemy"]),
            after: vec![(PROFILE, Some(new_profile.clone()))],
            ..open()
        },
        Case {
            name: "ufw: a new range under an existing rule updates it",
            files: vec![(UFW, exe()), (PROFILE, file(&profile(old_range())))],
            outputs: [ufw_active(), vec![out(UFW, "allow alchemy", 0, SKIPPED)]].concat(),
            commands: cmds(&["ufw status", "ufw allow alchemy", "ufw app update alchemy"]),
            after: vec![(PROFILE, Some(new_profile.clone()))],
            ..open()
        },
        Case {
            name: "ufw: the old profile without a rule needs no update",
            files: vec![(UFW, exe()), (PROFILE, file(&profile(old_range())))],
            outputs: [ufw_active(), vec![out(UFW, "allow alchemy", 0, ADDED)]].concat(),
            commands: cmds(&["ufw status", "ufw allow alchemy"]),
            ..open()
        },
        Case {
            name: "ufw: allow fails",
            files: vec![(UFW, exe())],
            outputs: [
                ufw_active(),
                vec![out(
                    UFW,
                    "allow alchemy",
                    1,
                    "ERROR: Could not find a profile matching 'alchemy'\n",
                )],
            ]
            .concat(),
            want: Err("command_failed"),
            commands: cmds(&["ufw status", "ufw allow alchemy"]),
            ..open()
        },
        Case {
            name: "ufw: status is not understood",
            files: vec![(UFW, exe())],
            outputs: vec![out(UFW, "status", 0, "Statut : actif\n")],
            want: Err("command_failed"),
            commands: cmds(&["ufw status"]),
            ..open()
        },
        Case {
            name: "ufw: status fails",
            files: vec![(UFW, exe())],
            outputs: vec![out(UFW, "status", 1, "")],
            want: Err("command_failed"),
            commands: cmds(&["ufw status"]),
            ..open()
        },
        Case {
            name: "ufw: writing the profile fails",
            files: vec![(UFW, exe())],
            outputs: ufw_active(),
            fail: Some(Call::WriteFile),
            want: Err("os_error"),
            commands: cmds(&["ufw status"]),
            ..open()
        },
        Case {
            name: "ufw: reading the profile fails",
            files: vec![(UFW, exe()), (PROFILE, FakeFile::symlink())],
            outputs: ufw_active(),
            want: Err("os_error"),
            commands: cmds(&["ufw status"]),
            ..open()
        },
        Case {
            name: "ufw: running it fails",
            files: vec![(UFW, exe())],
            fail: Some(Call::Run),
            want: Err("os_error"),
            ..open()
        },
        Case {
            name: "ufw not owned by root is refused",
            files: vec![(UFW, exe().owned_by(1000))],
            want: Err("refused"),
            ..open()
        },
        Case {
            name: "ufw writable by its group is refused",
            files: vec![(UFW, exe().with_mode(0o775))],
            want: Err("refused"),
            ..open()
        },
        Case {
            name: "ufw writable by others is refused",
            files: vec![(UFW, exe().with_mode(0o757))],
            want: Err("refused"),
            ..open()
        },
        Case {
            name: "ufw as a directory is refused",
            files: vec![(UFW, FakeFile::other())],
            want: Err("refused"),
            ..open()
        },
        Case {
            name: "a symlinked ufw is skipped",
            files: vec![(UFW, FakeFile::symlink())],
            want: Err("no_firewall"),
            ..open()
        },
        Case {
            name: "ufw in /sbin is found",
            files: vec![("/sbin/ufw", exe())],
            outputs: vec![out("/sbin/ufw", "status", 0, "Status: inactive\n")],
            want: Err("no_firewall"),
            commands: cmds(&["ufw status"]),
            ..open()
        },
        Case {
            name: "firewalld installed but stopped",
            files: vec![(FWCMD, exe())],
            outputs: vec![out(FWCMD, "--state", NOT_RUNNING_CODE, "not running\n")],
            want: Err("no_firewall"),
            commands: cmds(&["firewall-cmd --state"]),
            ..open()
        },
        Case {
            name: "firewalld: write, reload, and add the service",
            files: vec![(FWCMD, exe())],
            outputs: [
                fwd_running(),
                vec![
                    out(FWCMD, "--permanent --query-service=alchemy", 1, "no\n"),
                    out(FWCMD, "--query-service=alchemy", 1, "no\n"),
                ],
            ]
            .concat(),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --reload",
                "firewall-cmd --permanent --query-service=alchemy",
                "firewall-cmd --permanent --add-service=alchemy",
                "firewall-cmd --query-service=alchemy",
                "firewall-cmd --add-service=alchemy",
            ]),
            after: vec![(SERVICE, Some(new_service.clone()))],
            ..open()
        },
        Case {
            name: "firewalld: already open",
            files: vec![(FWCMD, exe()), (SERVICE, file(&new_service))],
            outputs: fwd_running(),
            want: Ok(false),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --info-service=alchemy",
                "firewall-cmd --permanent --query-service=alchemy",
                "firewall-cmd --query-service=alchemy",
            ]),
            after: vec![(SERVICE, Some(new_service.clone()))],
            ..open()
        },
        Case {
            name: "firewalld: a file it hasn't loaded is reloaded",
            files: vec![(FWCMD, exe()), (SERVICE, file(&new_service))],
            outputs: [
                fwd_running(),
                vec![out(
                    FWCMD,
                    "--info-service=alchemy",
                    101,
                    "Error: INVALID_SERVICE: alchemy\n",
                )],
            ]
            .concat(),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --info-service=alchemy",
                "firewall-cmd --reload",
                "firewall-cmd --permanent --query-service=alchemy",
                "firewall-cmd --query-service=alchemy",
            ]),
            ..open()
        },
        Case {
            name: "firewalld: a new range rewrites and reloads",
            files: vec![(FWCMD, exe()), (SERVICE, file(&service(old_range())))],
            outputs: fwd_running(),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --reload",
                "firewall-cmd --permanent --query-service=alchemy",
                "firewall-cmd --query-service=alchemy",
            ]),
            after: vec![(SERVICE, Some(new_service.clone()))],
            ..open()
        },
        Case {
            name: "firewalld: the reload fails",
            files: vec![(FWCMD, exe())],
            outputs: [fwd_running(), vec![out(FWCMD, "--reload", 101, "")]].concat(),
            want: Err("command_failed"),
            commands: cmds(&["firewall-cmd --state", "firewall-cmd --reload"]),
            ..open()
        },
        Case {
            name: "firewalld: a query errors",
            files: vec![(FWCMD, exe()), (SERVICE, file(&new_service))],
            outputs: [
                fwd_running(),
                vec![out(FWCMD, "--permanent --query-service=alchemy", 101, "")],
            ]
            .concat(),
            want: Err("command_failed"),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --info-service=alchemy",
                "firewall-cmd --permanent --query-service=alchemy",
            ]),
            ..open()
        },
        Case {
            name: "firewalld: --state is not understood",
            files: vec![(FWCMD, exe())],
            outputs: vec![out(FWCMD, "--state", 253, "Authorization failed.\n")],
            want: Err("command_failed"),
            commands: cmds(&["firewall-cmd --state"]),
            ..open()
        },
        Case {
            name: "firewalld in /usr/sbin is found",
            files: vec![("/usr/sbin/firewall-cmd", exe())],
            outputs: vec![out(
                "/usr/sbin/firewall-cmd",
                "--state",
                NOT_RUNNING_CODE,
                "",
            )],
            want: Err("no_firewall"),
            commands: cmds(&["firewall-cmd --state"]),
            ..open()
        },
        Case {
            name: "both active: both are opened",
            files: vec![(UFW, exe()), (FWCMD, exe())],
            outputs: [
                ufw_active(),
                fwd_running(),
                vec![out(UFW, "allow alchemy", 0, ADDED)],
            ]
            .concat(),
            commands: cmds(&[
                "ufw status",
                "firewall-cmd --state",
                "ufw allow alchemy",
                "firewall-cmd --reload",
                "firewall-cmd --permanent --query-service=alchemy",
                "firewall-cmd --query-service=alchemy",
            ]),
            after: vec![
                (PROFILE, Some(new_profile.clone())),
                (SERVICE, Some(new_service.clone())),
            ],
            ..open()
        },
        Case {
            name: "only the active one is opened",
            files: vec![(UFW, exe()), (FWCMD, exe())],
            outputs: [
                vec![out(UFW, "status", 0, "Status: inactive\n")],
                fwd_running(),
            ]
            .concat(),
            commands: cmds(&[
                "ufw status",
                "firewall-cmd --state",
                "firewall-cmd --reload",
                "firewall-cmd --permanent --query-service=alchemy",
                "firewall-cmd --query-service=alchemy",
            ]),
            after: vec![(PROFILE, None), (SERVICE, Some(new_service.clone()))],
            ..open()
        },
    ]);
}

const NOT_RUNNING_CODE: i32 = 252;

#[test]
#[allow(clippy::too_many_lines)]
fn close_on_linux() {
    let close = || case("", Op::Close);
    let zones = || out(FWCMD, "--permanent --get-zones", 0, "block home public\n");
    check(vec![
        Case {
            name: "no firewall installed",
            want: Ok(false),
            ..close()
        },
        Case {
            name: "ufw: delete the rule and the profile",
            files: vec![(UFW, exe()), (PROFILE, file(&profile(ProxyPorts::DEFAULT)))],
            outputs: vec![out(UFW, "delete allow alchemy", 0, DELETED)],
            commands: cmds(&["ufw delete allow alchemy"]),
            after: vec![(PROFILE, None)],
            ..close()
        },
        Case {
            name: "ufw: already closed",
            files: vec![(UFW, exe())],
            outputs: vec![out(UFW, "delete allow alchemy", 0, NOT_DELETED)],
            want: Ok(false),
            commands: cmds(&["ufw delete allow alchemy"]),
            ..close()
        },
        Case {
            name: "ufw: a rule left without its profile",
            files: vec![(UFW, exe())],
            outputs: vec![out(
                UFW,
                "delete allow alchemy",
                0,
                "Rules updated\nRules updated (v6)\n",
            )],
            commands: cmds(&["ufw delete allow alchemy"]),
            ..close()
        },
        Case {
            name: "ufw: a profile left without its rule",
            files: vec![(UFW, exe()), (PROFILE, file("old"))],
            outputs: vec![out(UFW, "delete allow alchemy", 0, NOT_DELETED)],
            commands: cmds(&["ufw delete allow alchemy"]),
            after: vec![(PROFILE, None)],
            ..close()
        },
        Case {
            name: "ufw: delete fails",
            files: vec![(UFW, exe()), (PROFILE, file("old"))],
            outputs: vec![out(UFW, "delete allow alchemy", 1, "")],
            want: Err("command_failed"),
            commands: cmds(&["ufw delete allow alchemy"]),
            after: vec![(PROFILE, Some("old".into()))],
            ..close()
        },
        Case {
            name: "ufw: removing the profile fails",
            files: vec![(UFW, exe()), (PROFILE, file("old"))],
            outputs: vec![out(UFW, "delete allow alchemy", 0, DELETED)],
            fail: Some(Call::RemoveFile),
            want: Err("os_error"),
            commands: cmds(&["ufw delete allow alchemy"]),
            ..close()
        },
        Case {
            name: "firewalld: remove from every zone, then the file, then reload",
            files: vec![
                (FWCMD, exe()),
                (SERVICE, file(&service(ProxyPorts::DEFAULT))),
            ],
            outputs: [
                fwd_running(),
                vec![
                    zones(),
                    out(
                        FWCMD,
                        "--permanent --zone=block --query-service=alchemy",
                        1,
                        "no\n",
                    ),
                    out(
                        FWCMD,
                        "--permanent --zone=home --query-service=alchemy",
                        0,
                        "yes\n",
                    ),
                    out(
                        FWCMD,
                        "--permanent --zone=public --query-service=alchemy",
                        0,
                        "yes\n",
                    ),
                ],
            ]
            .concat(),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --permanent --get-zones",
                "firewall-cmd --permanent --zone=block --query-service=alchemy",
                "firewall-cmd --permanent --zone=home --query-service=alchemy",
                "firewall-cmd --permanent --zone=home --remove-service=alchemy",
                "firewall-cmd --permanent --zone=public --query-service=alchemy",
                "firewall-cmd --permanent --zone=public --remove-service=alchemy",
                "firewall-cmd --reload",
            ]),
            after: vec![(SERVICE, None)],
            ..close()
        },
        Case {
            name: "firewalld: already closed",
            files: vec![(FWCMD, exe())],
            outputs: [
                fwd_running(),
                vec![
                    zones(),
                    out(
                        FWCMD,
                        "--permanent --zone=block --query-service=alchemy",
                        1,
                        "no\n",
                    ),
                    out(
                        FWCMD,
                        "--permanent --zone=home --query-service=alchemy",
                        1,
                        "no\n",
                    ),
                    out(
                        FWCMD,
                        "--permanent --zone=public --query-service=alchemy",
                        1,
                        "no\n",
                    ),
                ],
            ]
            .concat(),
            want: Ok(false),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --permanent --get-zones",
                "firewall-cmd --permanent --zone=block --query-service=alchemy",
                "firewall-cmd --permanent --zone=home --query-service=alchemy",
                "firewall-cmd --permanent --zone=public --query-service=alchemy",
            ]),
            ..close()
        },
        Case {
            name: "firewalld: a file no zone uses is deleted and reloaded",
            files: vec![(FWCMD, exe()), (SERVICE, file("old"))],
            outputs: [
                fwd_running(),
                vec![
                    out(FWCMD, "--permanent --get-zones", 0, "public\n"),
                    out(
                        FWCMD,
                        "--permanent --zone=public --query-service=alchemy",
                        1,
                        "no\n",
                    ),
                ],
            ]
            .concat(),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --permanent --get-zones",
                "firewall-cmd --permanent --zone=public --query-service=alchemy",
                "firewall-cmd --reload",
            ]),
            after: vec![(SERVICE, None)],
            ..close()
        },
        Case {
            name: "firewalld stopped with the service is unsupported",
            files: vec![(FWCMD, exe()), (SERVICE, file("old"))],
            outputs: vec![out(FWCMD, "--state", NOT_RUNNING_CODE, "not running\n")],
            want: Err("unsupported"),
            commands: cmds(&["firewall-cmd --state"]),
            after: vec![(SERVICE, Some("old".into()))],
            ..close()
        },
        Case {
            name: "firewalld stopped without the service",
            files: vec![(FWCMD, exe())],
            outputs: vec![out(FWCMD, "--state", NOT_RUNNING_CODE, "not running\n")],
            want: Ok(false),
            commands: cmds(&["firewall-cmd --state"]),
            ..close()
        },
        Case {
            name: "firewalld: removing from a zone fails",
            files: vec![(FWCMD, exe()), (SERVICE, file("old"))],
            outputs: [
                fwd_running(),
                vec![
                    out(FWCMD, "--permanent --get-zones", 0, "public\n"),
                    out(
                        FWCMD,
                        "--permanent --zone=public --remove-service=alchemy",
                        1,
                        "",
                    ),
                ],
            ]
            .concat(),
            want: Err("command_failed"),
            commands: cmds(&[
                "firewall-cmd --state",
                "firewall-cmd --permanent --get-zones",
                "firewall-cmd --permanent --zone=public --query-service=alchemy",
                "firewall-cmd --permanent --zone=public --remove-service=alchemy",
            ]),
            after: vec![(SERVICE, Some("old".into()))],
            ..close()
        },
        Case {
            name: "both: each is closed",
            files: vec![(UFW, exe()), (FWCMD, exe()), (PROFILE, file("p"))],
            outputs: [
                vec![out(UFW, "delete allow alchemy", 0, DELETED)],
                fwd_running(),
                vec![
                    out(FWCMD, "--permanent --get-zones", 0, "public\n"),
                    out(
                        FWCMD,
                        "--permanent --zone=public --query-service=alchemy",
                        1,
                        "no\n",
                    ),
                ],
            ]
            .concat(),
            commands: cmds(&[
                "ufw delete allow alchemy",
                "firewall-cmd --state",
                "firewall-cmd --permanent --get-zones",
                "firewall-cmd --permanent --zone=public --query-service=alchemy",
            ]),
            after: vec![(PROFILE, None)],
            ..close()
        },
    ]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn the_macos_application_firewall() {
    let open = || Case {
        platform: Platform::MacOs,
        files: vec![(SOCKETFILTERFW, exe()), (DAEMON, exe())],
        ..case("", Op::Open)
    };
    let close = || Case {
        platform: Platform::MacOs,
        files: vec![(SOCKETFILTERFW, exe())],
        ..case("", Op::Close)
    };
    let state = |stdout| out(SOCKETFILTERFW, "--getglobalstate", 0, stdout);
    let apps = |entries: &[(&str, bool)]| out(SOCKETFILTERFW, "--listapps", 0, &listing(entries));
    let others = [("/usr/sbin/cupsd", true), ("/usr/bin/ruby", false)];
    check(vec![
        Case {
            name: "open with the firewall off",
            outputs: vec![state(OFF)],
            want: Err("no_firewall"),
            commands: cmds(&["socketfilterfw --getglobalstate"]),
            ..open()
        },
        Case {
            name: "open adds and unblocks alchemyd",
            outputs: vec![
                state(ON),
                apps(&others),
                apps(&[others[0], others[1], (DAEMON, true)]),
            ],
            commands: cmds(&[
                "socketfilterfw --getglobalstate",
                "socketfilterfw --listapps",
                "socketfilterfw --add /opt/alchemy/bin/alchemyd",
                "socketfilterfw --unblockapp /opt/alchemy/bin/alchemyd",
                "socketfilterfw --listapps",
            ]),
            ..open()
        },
        Case {
            name: "open when blocking all incoming connections",
            outputs: vec![
                state("Firewall is blocking all non-essential incoming connections. (State = 2)\n"),
                apps(&[]),
                apps(&[(DAEMON, true)]),
            ],
            commands: cmds(&[
                "socketfilterfw --getglobalstate",
                "socketfilterfw --listapps",
                "socketfilterfw --add /opt/alchemy/bin/alchemyd",
                "socketfilterfw --unblockapp /opt/alchemy/bin/alchemyd",
                "socketfilterfw --listapps",
            ]),
            ..open()
        },
        Case {
            name: "open when already allowed",
            outputs: vec![state(ON), apps(&[(DAEMON, true)])],
            want: Ok(false),
            commands: cmds(&[
                "socketfilterfw --getglobalstate",
                "socketfilterfw --listapps",
            ]),
            ..open()
        },
        Case {
            name: "open unblocks a blocked entry",
            outputs: vec![state(ON), apps(&[(DAEMON, false)]), apps(&[(DAEMON, true)])],
            commands: cmds(&[
                "socketfilterfw --getglobalstate",
                "socketfilterfw --listapps",
                "socketfilterfw --unblockapp /opt/alchemy/bin/alchemyd",
                "socketfilterfw --listapps",
            ]),
            ..open()
        },
        Case {
            name: "open that doesn't take effect",
            outputs: vec![state(ON), apps(&others)],
            want: Err("command_failed"),
            commands: cmds(&[
                "socketfilterfw --getglobalstate",
                "socketfilterfw --listapps",
                "socketfilterfw --add /opt/alchemy/bin/alchemyd",
                "socketfilterfw --unblockapp /opt/alchemy/bin/alchemyd",
                "socketfilterfw --listapps",
            ]),
            ..open()
        },
        Case {
            name: "open with an unreadable state",
            outputs: vec![state("")],
            want: Err("command_failed"),
            commands: cmds(&["socketfilterfw --getglobalstate"]),
            ..open()
        },
        Case {
            name: "open without alchemyd",
            files: vec![(SOCKETFILTERFW, exe())],
            outputs: vec![state(ON)],
            want: Err("not_found"),
            commands: cmds(&["socketfilterfw --getglobalstate"]),
            ..open()
        },
        Case {
            name: "open refuses a symlinked alchemyd",
            files: vec![(SOCKETFILTERFW, exe()), (DAEMON, FakeFile::symlink())],
            outputs: vec![state(ON)],
            want: Err("refused"),
            commands: cmds(&["socketfilterfw --getglobalstate"]),
            ..open()
        },
        Case {
            name: "open without socketfilterfw",
            files: vec![(DAEMON, exe())],
            want: Err("no_firewall"),
            ..open()
        },
        Case {
            name: "close removes the entry",
            outputs: vec![apps(&[others[0], (DAEMON, true)]), apps(&others[..1])],
            commands: cmds(&[
                "socketfilterfw --listapps",
                "socketfilterfw --remove /opt/alchemy/bin/alchemyd",
                "socketfilterfw --listapps",
            ]),
            ..close()
        },
        Case {
            name: "close removes a blocked entry",
            outputs: vec![apps(&[(DAEMON, false)]), apps(&[])],
            commands: cmds(&[
                "socketfilterfw --listapps",
                "socketfilterfw --remove /opt/alchemy/bin/alchemyd",
                "socketfilterfw --listapps",
            ]),
            ..close()
        },
        Case {
            name: "close when not listed",
            outputs: vec![apps(&others)],
            want: Ok(false),
            commands: cmds(&["socketfilterfw --listapps"]),
            ..close()
        },
        Case {
            name: "close that doesn't take effect",
            outputs: vec![apps(&[(DAEMON, true)])],
            want: Err("command_failed"),
            commands: cmds(&[
                "socketfilterfw --listapps",
                "socketfilterfw --remove /opt/alchemy/bin/alchemyd",
                "socketfilterfw --listapps",
            ]),
            ..close()
        },
        Case {
            name: "close without socketfilterfw",
            files: vec![],
            want: Ok(false),
            ..close()
        },
    ]);
}

#[test]
fn proxy_ports_parse_only_canonical_ranges() {
    let cases = [
        ("8100-8999", Ok((8100, 8999))),
        ("1024-1024", Ok((1024, 1024))),
        ("1024-65535", Ok((1024, 65535))),
        ("1023-8999", Err(ProxyPortsError::Privileged(1023))),
        ("0-0", Err(ProxyPortsError::Privileged(0))),
        (
            "8999-8100",
            Err(ProxyPortsError::Reversed {
                start: 8999,
                end: 8100,
            }),
        ),
        ("8100", Err(ProxyPortsError::Format)),
        ("8100-", Err(ProxyPortsError::Format)),
        ("-8999", Err(ProxyPortsError::Format)),
        ("8100:8999", Err(ProxyPortsError::Format)),
        ("8100-8999-9000", Err(ProxyPortsError::Format)),
        ("08100-8999", Err(ProxyPortsError::Format)),
        ("+8100-8999", Err(ProxyPortsError::Format)),
        ("8100 -8999", Err(ProxyPortsError::Format)),
        ("8100-8999\n", Err(ProxyPortsError::Format)),
        ("8100-65536", Err(ProxyPortsError::Format)),
        ("８１００-8999", Err(ProxyPortsError::Format)),
        ("", Err(ProxyPortsError::Format)),
    ];
    for (input, want) in cases {
        let got = input.parse::<ProxyPorts>().map(|p| (p.start(), p.end()));
        assert_eq!(got, want, "{input:?}");
    }
}

#[test]
fn proxy_ports_render_per_firewall() {
    let cases = [
        (ProxyPorts::DEFAULT, "8100-8999", "8100:8999", "8100-8999"),
        (
            ProxyPorts::new(9000, 9000).unwrap(),
            "9000-9000",
            "9000",
            "9000",
        ),
    ];
    for (ports, display, ufw, firewalld) in cases {
        assert_eq!(ports.to_string(), display);
        assert_eq!(ports.ufw(), ufw);
        assert_eq!(ports.firewalld(), firewalld);
        assert_eq!(display.parse::<ProxyPorts>().unwrap(), ports);
    }
    assert_eq!(ProxyPorts::default(), ProxyPorts::DEFAULT);
}

#[test]
fn the_ufw_profile() {
    assert_eq!(
        profile(ProxyPorts::DEFAULT),
        "[alchemy]\n\
         title=Alchemy\n\
         description=Alchemy reverse proxy (80), app ports (8100-8999) and mDNS (5353/udp)\n\
         ports=80/tcp|8100:8999/tcp|5353/udp\n"
    );
    assert!(
        profile(ProxyPorts::new(9000, 9000).unwrap()).contains("ports=80/tcp|9000/tcp|5353/udp\n")
    );
}

#[test]
fn the_firewalld_service() {
    assert_eq!(
        service(ProxyPorts::DEFAULT),
        r#"<?xml version="1.0" encoding="utf-8"?>
<service>
  <short>Alchemy</short>
  <description>Alchemy reverse proxy (80), app ports (8100-8999) and mDNS (5353/udp).</description>
  <port protocol="tcp" port="80"/>
  <port protocol="tcp" port="8100-8999"/>
  <port protocol="udp" port="5353"/>
</service>
"#
    );
}

#[test]
fn global_states() {
    let cases = [
        ("Firewall is disabled. (State = 0)\n", Some(0)),
        ("Firewall is enabled. (State = 1)\n", Some(1)),
        (
            "Firewall is blocking all non-essential incoming connections. (State = 2)",
            Some(2),
        ),
        ("Firewall is enabled.\n", None),
        ("(State = x)", None),
        ("", None),
    ];
    for (stdout, want) in cases {
        assert_eq!(global_state(&output(0, stdout)), want, "{stdout:?}");
    }
}

#[test]
fn listapps_entries() {
    let listing = "Total number of apps = 3 \n\
                   1 : /usr/local/libexec/remotepairingdeviced \n             \
                   (Allow incoming connections)\n\
                   2 : /Applications/My App.app \n             \
                   (Block incoming connections)\n\
                   3 : /opt/alchemy/bin/alchemyd \n             \
                   (Allow incoming connections)\n";
    assert_eq!(
        apps(listing),
        [
            ("/usr/local/libexec/remotepairingdeviced".to_owned(), true),
            ("/Applications/My App.app".to_owned(), false),
            ("/opt/alchemy/bin/alchemyd".to_owned(), true),
        ]
    );
    assert_eq!(apps("Total number of apps = 0 \n"), []);
    assert_eq!(
        apps("1 : /a \n2 : /b \n  (Block incoming connections)\n"),
        [("/b".to_owned(), false)]
    );
}

/// The real `--listapps` output parses, and holds as many entries as it
/// says. Listing needs no root.
#[cfg(target_os = "macos")]
#[test]
fn the_real_listapps_parses() {
    let out = crate::RealSystem
        .run(&crate::system::FixedCommand::new(SOCKETFILTERFW, vec!["--listapps".into()]).unwrap())
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let total: usize = stdout
        .split_once("= ")
        .and_then(|(_, rest)| rest.split_whitespace().next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{stdout}"));
    assert_eq!(apps(&stdout).len(), total, "{stdout}");
    let state = crate::RealSystem
        .run(
            &crate::system::FixedCommand::new(SOCKETFILTERFW, vec!["--getglobalstate".into()])
                .unwrap(),
        )
        .unwrap();
    assert!(global_state(&state).is_some());
}

#[test]
fn errors_explain_what_to_do() {
    let sys = FakeSystem::new(HELPER).with_file(UFW, exe().owned_by(501));
    let err = open(&sys, Platform::Linux, ProxyPorts::DEFAULT).unwrap_err();
    assert_eq!(
        err.to_string(),
        "/usr/sbin/ufw: refusing to run it: it is not owned by root"
    );

    let sys = FakeSystem::new(HELPER)
        .with_file(FWCMD, exe())
        .with_file(SERVICE, file("x"))
        .responding(&[FWCMD, "--state"], NOT_RUNNING_CODE, "not running\n");
    let err = close(&sys, Platform::Linux, ProxyPorts::DEFAULT).unwrap_err();
    assert_eq!(
        err.to_string(),
        "/etc/firewalld/services/alchemy.xml: firewalld is not running, so its rules for Alchemy cannot be removed; start it and try again"
    );
}
