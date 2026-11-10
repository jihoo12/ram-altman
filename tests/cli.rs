// Exercises the binary with simulated memory and a fake sendmail.
// No inboxes are harmed.

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct Sandbox {
    dir: PathBuf,
    state: PathBuf,
    meminfo: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Sandbox {
        let unique = format!(
            "ram-altman-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        );
        let dir = std::env::temp_dir().join(unique);
        let state = dir.join("state");
        let bin = dir.join("bin");

        fs::create_dir_all(&state).unwrap();
        fs::create_dir_all(&bin).unwrap();

        let sendmail = dir.join("bin/sendmail");
        let script = format!(
            "#!/bin/sh\nif [ -e '{}/fail' ]; then exit 75; fi\ncat >> '{}/outbox'\necho '=====END=====' >> '{}/outbox'\n",
            dir.display(),
            dir.display(),
            dir.display()
        );
        fs::write(&sendmail, script).unwrap();
        fs::set_permissions(&sendmail, fs::Permissions::from_mode(0o755)).unwrap();

        Sandbox {
            meminfo: dir.join("meminfo"),
            dir,
            state,
            bin,
        }
    }

    fn cmd(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ram-altman"));
        let mut path = self.bin.display().to_string();
        if let Some(existing) = std::env::var_os("PATH") {
            path.push(':');
            path.push_str(&existing.to_string_lossy());
        }

        command
            .env("PATH", path)
            .env("RAM_ALTMAN_TO", "to@example.com")
            .env("RAM_ALTMAN_FROM", "from@example.com")
            .env("RAM_ALTMAN_STATE_DIR", &self.state)
            .env("RAM_ALTMAN_MEMINFO", &self.meminfo)
            .env_remove("RAM_ALTMAN_FROM_NAME")
            .env_remove("RAM_ALTMAN_THRESHOLD");
        command
    }

    fn run(&self) -> Output {
        self.cmd().output().unwrap()
    }

    fn run_arg(&self, arg: &str) -> Output {
        self.cmd().arg(arg).output().unwrap()
    }

    #[allow(clippy::too_many_arguments)]
    fn mem(
        &self,
        total: u64,
        free: u64,
        available: Option<u64>,
        buffers: u64,
        cached: u64,
        slab: u64,
        shmem: u64,
    ) {
        let mut text = format!("MemTotal:       {total} kB\nMemFree:        {free} kB\n");
        if let Some(available) = available {
            text.push_str(&format!("MemAvailable:   {available} kB\n"));
        }
        text.push_str(&format!(
            "Buffers:        {buffers} kB\nCached:         {cached} kB\nSwapCached:     999999 kB\nSReclaimable:   {slab} kB\nShmem:          {shmem} kB\n"
        ));
        fs::write(&self.meminfo, text).unwrap();
    }

    fn sent(&self) -> usize {
        self.outbox().matches("=====END=====\n").count()
    }

    fn outbox(&self) -> String {
        fs::read_to_string(self.dir.join("outbox")).unwrap_or_default()
    }

    fn last_sent(&self) -> String {
        fs::read_to_string(self.state.join("last_sent"))
            .map(|text| text.trim_end_matches('\n').to_string())
            .unwrap_or_default()
    }

    fn set_last_sent(&self, value: &str) {
        fs::write(self.state.join("last_sent"), format!("{value}\n")).unwrap();
    }

    fn usage(&self) -> String {
        fs::read_to_string(self.state.join("usage.csv")).unwrap_or_default()
    }

    fn fail_send(&self, fail: bool) {
        let marker = self.dir.join("fail");
        if fail {
            fs::write(marker, "").unwrap();
        } else {
            let _ = fs::remove_file(marker);
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn assert_ok(output: &Output) {
    assert!(
        output.status.success(),
        "program failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn cache_counts_as_available_and_only_one_email_a_day() {
    let sandbox = Sandbox::new();
    sandbox.mem(
        16_000_000,
        300_000,
        Some(9_000_000),
        200_000,
        8_000_000,
        500_000,
        100_000,
    );

    assert_ok(&sandbox.run());
    assert_eq!(sandbox.sent(), 1, "cache counts as available: email sent");

    assert_ok(&sandbox.run());
    assert_ok(&sandbox.run());
    assert_eq!(
        sandbox.sent(),
        1,
        "second and third run same day: no more email"
    );

    let usage = sandbox.usage();
    let last = usage.lines().last().unwrap();
    assert_eq!(last.split(',').nth(2).unwrap(), "7000000");
    assert_eq!(usage.lines().count() - 1, 3);
}

#[test]
fn ten_percent_is_not_over_ten_percent() {
    let sandbox = Sandbox::new();
    sandbox.mem(
        16_000_000,
        300_000,
        Some(9_000_000),
        200_000,
        8_000_000,
        500_000,
        100_000,
    );
    assert_ok(&sandbox.run());
    assert_eq!(sandbox.sent(), 1);

    // Next day, tight on memory.
    sandbox.set_last_sent("2000-01-01");
    sandbox.mem(
        16_000_000,
        300_000,
        Some(1_000_000),
        100_000,
        800_000,
        100_000,
        100_000,
    );
    assert_ok(&sandbox.run());
    assert_eq!(sandbox.sent(), 1, "6.2% available: no email");

    // Exactly 10% is not over 10%.
    sandbox.mem(
        16_000_000,
        300_000,
        Some(1_600_000),
        100_000,
        800_000,
        100_000,
        100_000,
    );
    assert_ok(&sandbox.run());
    assert_eq!(sandbox.sent(), 1, "exactly 10%: no email");

    sandbox.mem(
        16_000_000,
        300_000,
        Some(1_600_001),
        100_000,
        800_000,
        100_000,
        100_000,
    );
    assert_ok(&sandbox.run());
    assert_eq!(sandbox.sent(), 2, "just over 10%: email sent");
}

#[test]
fn failed_send_is_retried_and_only_recorded_once_it_works() {
    let sandbox = Sandbox::new();
    sandbox.mem(
        16_000_000,
        300_000,
        Some(9_000_000),
        200_000,
        8_000_000,
        500_000,
        100_000,
    );
    assert_ok(&sandbox.run());
    assert_eq!(sandbox.sent(), 1);

    sandbox.set_last_sent("2000-01-01");
    sandbox.fail_send(true);
    let failed = sandbox.run();
    assert_eq!(failed.status.code(), Some(1), "failed send exits non-zero");
    assert_eq!(
        sandbox.last_sent(),
        "2000-01-01",
        "failed send not recorded"
    );

    sandbox.fail_send(false);
    assert_ok(&sandbox.run());
    assert_eq!(sandbox.sent(), 2, "retry succeeds");
}

#[test]
fn old_kernel_falls_back_to_free_plus_reclaimable_cache() {
    let sandbox = Sandbox::new();
    // SwapCached in the fixture must not be picked up as Cached.
    sandbox.mem(
        16_000_000, 1_000_000, None, 200_000, 3_000_000, 500_000, 700_000,
    );
    let output = sandbox.run_arg("--status");
    assert_ok(&output);
    assert!(
        stdout(&output).contains("Available:  3.8 GiB"),
        "fallback estimate: {}",
        stdout(&output)
    );
}

#[test]
fn every_opening_line_appears_without_back_to_back_repeats() {
    let sandbox = Sandbox::new();
    sandbox.mem(
        16_000_000,
        300_000,
        Some(9_000_000),
        200_000,
        8_000_000,
        500_000,
        100_000,
    );

    for _ in 0..40 {
        sandbox.set_last_sent("2000-01-01");
        assert_ok(&sandbox.run());
    }

    let outbox = sandbox.outbox();
    let bodies: Vec<&str> = outbox
        .split("=====END=====\n")
        .filter(|message| !message.is_empty())
        .filter_map(|message| {
            message
                .split("Hi Sam,\n\n")
                .nth(1)
                .and_then(|rest| rest.split("\n\n").next())
        })
        .collect();

    assert_eq!(bodies.len(), 40);
    let first_round: HashSet<&str> = bodies.iter().take(10).copied().collect();
    assert_eq!(first_round.len(), 10, "all opening lines used");
    for pair in bodies.windows(2) {
        assert_ne!(pair[0], pair[1], "no back-to-back repeat");
    }
}

#[test]
fn missing_recipient_is_refused() {
    let sandbox = Sandbox::new();
    let output = sandbox.cmd().env("RAM_ALTMAN_TO", "").output().unwrap();
    assert_eq!(output.status.code(), Some(1), "missing recipient refused");
}

#[test]
fn status_reports_the_evidence() {
    let sandbox = Sandbox::new();
    sandbox.mem(
        16_000_000,
        300_000,
        Some(9_000_000),
        200_000,
        8_000_000,
        500_000,
        100_000,
    );
    let output = sandbox.run_arg("--status");
    assert_ok(&output);
    let text = stdout(&output);

    for expected in [
        "Total:      15.3 GiB",
        "Used:       6.7 GiB",
        "Available:  8.6 GiB (56.2%)",
        "  Free:     0.3 GiB",
        "  Cache:    8.2 GiB",
    ] {
        assert!(text.contains(expected), "missing {expected:?} in:\n{text}");
    }
}

#[test]
fn dry_run_sends_nothing_and_writes_nothing() {
    let sandbox = Sandbox::new();
    sandbox.mem(
        16_000_000,
        300_000,
        Some(9_000_000),
        200_000,
        8_000_000,
        500_000,
        100_000,
    );

    let output = sandbox.run_arg("--dry-run");
    assert_ok(&output);
    let text = stdout(&output);
    assert!(text.contains("Subject: "));
    assert!(text.contains("From: sender-not-set@example.com"));
    assert!(text.contains("To: recipient-not-set@example.com"));

    assert_eq!(sandbox.sent(), 0);
    assert_eq!(
        fs::read_dir(&sandbox.state).unwrap().count(),
        0,
        "dry run left state behind"
    );
}

#[test]
fn help_and_unknown_options() {
    let sandbox = Sandbox::new();

    let help = sandbox.run_arg("--help");
    assert_ok(&help);
    assert!(stdout(&help).contains("Usage:"));

    let unknown = sandbox.run_arg("--bogus");
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("unknown option: --bogus"),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&unknown.stderr)
    );
}
