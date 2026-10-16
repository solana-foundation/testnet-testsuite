//! What `scenario.sh` left on disk: pid files, service logs, scale overlays.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result};
use control_client::Process;

/// Services `scenario.sh` starts, in the order the UI lists them.
pub const SERVICES: [&str; 9] = [
    "chain-watch",
    "oracle",
    "router",
    "mm",
    "flow",
    "arb",
    "prometheus",
    "grafana",
    "node_exporter",
];

/// Each known service with its pid file's process, alive only if that pid
/// still runs that binary (scenario.sh's `running` rule: a reused pid
/// doesn't count).
pub fn processes(state_dir: &Path) -> Vec<Process> {
    SERVICES
        .iter()
        .map(|name| {
            let pid = std::fs::read_to_string(state_dir.join("pids").join(name))
                .ok()
                .and_then(|pid| pid.trim().parse::<u32>().ok());
            let running = pid.is_some_and(|pid| {
                std::fs::read(format!("/proc/{pid}/cmdline"))
                    .map(|cmdline| String::from_utf8_lossy(&cmdline).contains(name))
                    .unwrap_or(false)
            });
            Process {
                name: (*name).to_owned(),
                pid,
                running,
            }
        })
        .collect()
}

/// The last `lines` lines of a service's log, ANSI colours removed. Reads
/// at most the final 512 KiB.
pub fn tail(state_dir: &Path, name: &str, lines: usize) -> Result<String> {
    anyhow::ensure!(SERVICES.contains(&name), "unknown service {name}");
    let path = state_dir.join("logs").join(format!("{name}.log"));
    let mut file = std::fs::File::open(&path).with_context(|| format!("{}", path.display()))?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(512 * 1024);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    let text = strip_ansi(&String::from_utf8_lossy(&bytes));
    let all: Vec<&str> = text.lines().collect();
    // A partial first line from the seek is dropped.
    let skip = usize::from(start > 0);
    let from = all.len().saturating_sub(lines).max(skip);
    Ok(all.get(from..).unwrap_or_default().join("\n"))
}

/// Scale overlay names (`config/scale-<name>.toml`), sorted.
pub fn scales(config_dir: &Path) -> Vec<String> {
    let mut scales: Vec<String> = std::fs::read_dir(config_dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name().into_string().ok()?;
            Some(
                name.strip_prefix("scale-")?
                    .strip_suffix(".toml")?
                    .to_owned(),
            )
        })
        .collect();
    scales.sort();
    scales
}

/// Remove ANSI escape sequences (CSI `ESC [ ... final`, and lone `ESC x`).
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        if chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if ('@'..='~').contains(&c) {
                    break;
                }
            }
        } else {
            chars.next();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_colours() {
        assert_eq!(
            strip_ansi("\u{1b}[2m2026\u{1b}[0m \u{1b}[32m INFO\u{1b}[0m ok"),
            "2026  INFO ok"
        );
    }

    #[test]
    fn tails_logs_and_reads_pids() {
        let dir = std::env::temp_dir().join(format!("control-proc-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("logs")).expect("dir");
        std::fs::create_dir_all(dir.join("pids")).expect("dir");
        std::fs::write(dir.join("logs/mm.log"), "a\nb\n\u{1b}[1mc\u{1b}[0m\n").expect("log");
        assert_eq!(tail(&dir, "mm", 2).expect("tail"), "b\nc");
        assert!(tail(&dir, "../etc/passwd", 2).is_err());
        // Our own pid runs a binary whose name is not "mm": not running.
        std::fs::write(dir.join("pids/mm"), std::process::id().to_string()).expect("pid");
        let mm = processes(&dir)
            .into_iter()
            .find(|p| p.name == "mm")
            .expect("mm");
        assert_eq!(mm.pid, Some(std::process::id()));
        assert!(!mm.running);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finds_scales() {
        let scales = scales(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../config"
        )));
        assert!(scales.contains(&"x10".to_owned()), "{scales:?}");
    }
}
