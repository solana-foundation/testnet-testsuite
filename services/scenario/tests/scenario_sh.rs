//! `bootstrap/scenario/scenario.sh` program handling, run against stubbed
//! `solana`, `curl`, `cargo` and `scenario` binaries: the janitor teardown
//! restores the forks whatever fails, and `deploy` / `up` refuse programs
//! that do not run their fork artifact.
//!
//! The script is copied into a scratch tree, so its `.scenario` state lives
//! there and never touches a running scenario.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const PROGRAMS: [(&str, &str); 3] = [
    ("KEYPAIR_RAYDIUM_CLMM", "RaydiumProgram"),
    ("KEYPAIR_ORCA_WHIRLPOOL", "OrcaProgram"),
    ("KEYPAIR_MANIFEST", "ManifestProgram"),
];

struct Tree {
    root: PathBuf,
}

impl Drop for Tree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write(path: &Path, text: &str, executable: bool) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(path, text).expect("write");
    if executable {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
}

/// The script plus stubs. `solana program deploy` appends
/// "<program id> <artifact>" to `deploys.log` and fails for an artifact
/// named in `$FAIL_DEPLOY` (only for `$FAIL_KEY`, when set);
/// `scenario check-programs` passes for programs still running what was
/// last deployed there successfully (their fork, initially), or exits
/// `$CHECK_STATUS` when set; `scenario janitor` exits `$JANITOR_STATUS`.
fn tree(name: &str) -> Option<Tree> {
    for tool in ["bash", "jq"] {
        if Command::new(tool).arg("--version").output().is_err() {
            eprintln!("skipping: {tool} not installed");
            return None;
        }
    }
    let root = std::env::temp_dir().join(format!(
        "mtm-scenario-sh-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bootstrap/scenario/scenario.sh");
    write(
        &root.join("bootstrap/scenario/scenario.sh"),
        &std::fs::read_to_string(script).expect("scenario.sh"),
        true,
    );
    let stubs = root.join("stubs");
    write(
        &stubs.join("solana"),
        r#"#!/usr/bin/env bash
case "$1" in
  --version) echo "solana-cli 4.4.0 (src:stub)" ;;
  program)
    if [[ "$2" == show ]]; then echo "Program Id: $5"; exit 0; fi
    args=("$@"); artifact="${args[-1]}"; key=""
    for ((i = 0; i < ${#args[@]}; i++)); do
      [[ "${args[i]}" == --program-id ]] && key="$(cat "${args[i + 1]}")"
    done
    echo "$key $artifact" >>"$LOG_DIR/deploys.log"
    if [[ -n "${FAIL_DEPLOY:-}" && "$artifact" == *"$FAIL_DEPLOY"* \
      && ( -z "${FAIL_KEY:-}" || "$key" == "$FAIL_KEY" ) ]]; then exit 1; fi
    mkdir -p "$LOG_DIR/code"; basename "$artifact" >"$LOG_DIR/code/$key"
    exit 0 ;;
esac
"#,
        true,
    );
    write(
        &stubs.join("curl"),
        "#!/usr/bin/env bash\necho '{\"jsonrpc\":\"2.0\",\"result\":{\"value\":{\"lamports\":1}}}'\n",
        true,
    );
    write(&stubs.join("cargo"), "#!/usr/bin/env bash\nexit 0\n", true);
    write(
        &root.join("target/stub/scenario"),
        r#"#!/usr/bin/env bash
echo "$*" >>"$LOG_DIR/scenario.log"
case "$1" in
  check-programs)
    [[ -n "${CHECK_STATUS:-}" ]] && exit "$CHECK_STATUS"
    # Each program runs its fork until a deploy changed it.
    shift
    while (( $# )); do
      if [[ "$1" == --program ]]; then
        key="${2%%=*}" want="$(basename "${2#*=}")"
        [[ ! -f "$LOG_DIR/code/$key" || "$(cat "$LOG_DIR/code/$key")" == "$want" ]] || exit 1
        shift
      fi
      shift
    done
    exit 0 ;;
  janitor)
    if [[ -n "${JANITOR_SLEEP:-}" ]]; then sleep "$JANITOR_SLEEP"; fi
    exit "${JANITOR_STATUS:-0}" ;;
  *) echo '{}' ;;
esac
"#,
        true,
    );
    for artifact in ["janitor.so", "raydium.so", "orca.so", "manifest.so"] {
        write(&root.join("so").join(artifact), "\x7fELF", false);
    }
    Some(Tree { root })
}

fn command(tree: &Tree, args: &[&str], env: &[(&str, &str)]) -> Command {
    let root = &tree.root;
    let path = format!(
        "{}:{}",
        root.join("stubs").display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut command = Command::new("bash");
    command
        .arg(root.join("bootstrap/scenario/scenario.sh"))
        .args(args)
        .env_clear()
        .env("PATH", path)
        .env("HOME", root)
        .env("LOG_DIR", root)
        .env("CARGO_TARGET_DIR", root.join("target"))
        .env("SCENARIO_BUILD", "stub")
        .env("KEYPAIR_FAUCET", "Faucet")
        .env("KEYPAIR_PROGRAM_AUTHORITY", "Authority")
        .env("SCENARIO_JANITOR_SO", root.join("so/janitor.so"))
        .env("SCENARIO_RAYDIUM_SO", root.join("so/raydium.so"))
        .env("SCENARIO_ORCA_SO", root.join("so/orca.so"))
        .env("SCENARIO_MANIFEST_SO", root.join("so/manifest.so"));
    for (var, key) in PROGRAMS {
        command.env(var, key);
    }
    for (key, value) in env {
        command.env(key, value);
    }
    command
}

fn run(tree: &Tree, args: &[&str], env: &[(&str, &str)]) -> (Output, Vec<String>) {
    let output = command(tree, args, env).output().expect("bash");
    (output, deploys(tree))
}

/// `deploys.log` as "<program id> <artifact file name>" lines.
fn deploys(tree: &Tree) -> Vec<String> {
    std::fs::read_to_string(tree.root.join("deploys.log"))
        .unwrap_or_default()
        .lines()
        .map(|line| {
            // "<key> <path>" → "<key> <file name>"
            let (key, path) = line.split_once(' ').expect("log line");
            let file = Path::new(path).file_name().expect("file").to_string_lossy();
            format!("{key} {file}")
        })
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

const JANITOR: [&str; 3] = [
    "RaydiumProgram janitor.so",
    "OrcaProgram janitor.so",
    "ManifestProgram janitor.so",
];
const FORKS: [&str; 3] = [
    "RaydiumProgram raydium.so",
    "OrcaProgram orca.so",
    "ManifestProgram manifest.so",
];

fn teardown(env: &[(&str, &str)]) -> Option<(Output, Vec<String>)> {
    let tree = tree("teardown")?;
    let mut all = vec![("SCENARIO_JANITOR", "1")];
    all.extend_from_slice(env);
    Some(run(&tree, &["teardown", "local"], &all))
}

#[test]
fn a_failed_janitor_run_still_restores_every_fork() {
    let Some((output, deploys)) = teardown(&[("JANITOR_STATUS", "1")]) else {
        return;
    };
    assert!(!output.status.success(), "{}", stderr(&output));
    assert_eq!(deploys, [JANITOR, FORKS].concat(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("the forks are restored"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_failed_restore_does_not_stop_the_others_and_fails_the_run() {
    let Some((output, deploys)) = teardown(&[("FAIL_DEPLOY", "orca.so")]) else {
        return;
    };
    assert!(!output.status.success(), "{}", stderr(&output));
    assert_eq!(deploys, [JANITOR, FORKS].concat(), "{}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.contains("NOT restored") && err.contains("KEYPAIR_ORCA_WHIRLPOOL"),
        "{err}"
    );
    assert!(!err.contains("KEYPAIR_MANIFEST;"), "{err}");
}

#[test]
fn a_failed_janitor_upgrade_restores_what_was_upgraded() {
    // Orca's janitor upgrade fails: set -e stops there with only Raydium
    // running the janitor, and only Raydium is redeployed.
    let Some((output, deploys)) =
        teardown(&[("FAIL_DEPLOY", "janitor.so"), ("FAIL_KEY", "OrcaProgram")])
    else {
        return;
    };
    assert!(!output.status.success(), "{}", stderr(&output));
    assert_eq!(
        deploys,
        [&JANITOR[..2], &FORKS[..1]].concat(),
        "{}",
        stderr(&output)
    );
}

#[test]
fn ctrl_c_during_the_janitor_run_restores_every_fork() {
    use std::os::unix::process::CommandExt;
    let Some(tree) = tree("interrupt") else {
        return;
    };
    let child = command(
        &tree,
        &["teardown", "local"],
        &[("SCENARIO_JANITOR", "1"), ("JANITOR_SLEEP", "30")],
    )
    .process_group(0)
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::piped())
    .spawn()
    .expect("bash");
    let started = std::time::Instant::now();
    while !std::fs::read_to_string(tree.root.join("scenario.log"))
        .unwrap_or_default()
        .contains("janitor --janitor-so")
    {
        assert!(started.elapsed().as_secs() < 20, "the janitor never ran");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    // What a terminal's Ctrl-C does: SIGINT to the whole process group.
    let group = format!("-{}", child.id());
    let killed = Command::new("kill")
        .args(["-INT", "--", &group])
        .status()
        .expect("kill");
    assert!(killed.success());
    let output = child.wait_with_output().expect("wait");
    assert!(!output.status.success(), "{}", stderr(&output));
    assert_eq!(
        deploys(&tree),
        [JANITOR, FORKS].concat(),
        "{}",
        stderr(&output)
    );
    assert!(
        started.elapsed().as_secs() < 20,
        "the stub was not interrupted"
    );
}

#[test]
fn a_clean_janitor_run_restores_the_forks_and_finishes_the_teardown() {
    let Some((output, deploys)) = teardown(&[]) else {
        return;
    };
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(deploys, [JANITOR, FORKS].concat());
    assert!(stderr(&output).contains("forks restored with no accounts"));
}

#[test]
fn deploy_refuses_a_program_that_does_not_run_its_fork_unless_asked() {
    let Some(tree) = tree("deploy") else {
        return;
    };
    let (output, deploys) = run(&tree, &["deploy", "local"], &[("CHECK_STATUS", "1")]);
    assert!(!output.status.success());
    assert!(deploys.is_empty(), "{deploys:?}");
    assert!(stderr(&output).contains("refusing"), "{}", stderr(&output));

    let (output, deploys) = run(
        &tree,
        &["deploy", "local"],
        &[("CHECK_STATUS", "1"), ("SCENARIO_UPGRADE_PROGRAMS", "1")],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(deploys, FORKS);

    // Every program runs its fork: nothing to deploy.
    std::fs::remove_file(tree.root.join("deploys.log")).expect("rm");
    let (output, deploys) = run(&tree, &["deploy", "local"], &[]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(deploys.is_empty(), "{deploys:?}");
}

#[test]
fn up_refuses_before_starting_anything() {
    let Some(tree) = tree("up") else {
        return;
    };
    let (output, _) = run(&tree, &["up", "local"], &[("CHECK_STATUS", "1")]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("Nothing was started"),
        "{}",
        stderr(&output)
    );
    let calls = std::fs::read_to_string(tree.root.join("scenario.log")).expect("log");
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(
        calls.starts_with("check-programs --program RaydiumProgram="),
        "{calls}"
    );
    assert!(calls.contains("--janitor-so"), "{calls}");
}
