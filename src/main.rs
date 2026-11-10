// ram-altman - track RAM usage and, at most once a day, tell Sam Altman
// when this PC has RAM to spare.
//
// Usage:
//   ram-altman            Log a sample and send the daily email if due
//   ram-altman --status   Print current memory figures and exit
//   ram-altman --dry-run  Print the email that would be sent and exit
//
// Cron (every 5 minutes):
//   */5 * * * * /path/to/ram-altman
//
// How "used" is worked out:
//   used = MemTotal - MemAvailable
//
//   MemAvailable is the kernel's own estimate of how much memory can be handed
//   to new programs without swapping. It covers free memory plus the page cache
//   and slab memory that can be reclaimed, so cached RAM does not count as used.
//   It leaves out what can't really be reclaimed, such as tmpfs/shared memory,
//   which makes it more accurate than simply adding "free" and "cached" together.

use std::env;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::raw::{c_char, c_int, c_long};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 10 * 1024 * 1024;
const LOG_LINES_TO_KEEP: usize = 50_000;

const HELP: &str = "\
ram-altman - track RAM usage and, at most once a day, tell Sam Altman
when this PC has RAM to spare.

Usage:
  ram-altman            Log a sample and send the daily email if due
  ram-altman --status   Print current memory figures and exit
  ram-altman --dry-run  Print the email that would be sent and exit

Cron (every 5 minutes):
  */5 * * * * /path/to/ram-altman
";

fn main() {
    unsafe { tzset() };

    let result = run();
    if let Err(message) = result {
        eprintln!("ram-altman: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let arg = env::args().nth(1).unwrap_or_default();
    match arg.as_str() {
        "--status" => {
            let memory = read_memory(&meminfo_path())?;
            print_status(&memory);
            Ok(())
        }
        "--dry-run" => dry_run(),
        "-h" | "--help" => {
            print!("{HELP}");
            Ok(())
        }
        "" => run_scheduled(),
        other => Err(format!("unknown option: {other} (try --help)")),
    }
}

/// The cron path: log a sample and, if it is due, send the daily email.
fn run_scheduled() -> Result<(), String> {
    let recipient = nonempty_env("RAM_ALTMAN_TO").ok_or("RECIPIENT is not set")?;
    let sender = nonempty_env("RAM_ALTMAN_FROM").ok_or("SENDER is not set")?;
    let sender_name = nonempty_env("RAM_ALTMAN_FROM_NAME");

    let state_dir = state_dir_path()?;
    fs::create_dir_all(&state_dir)
        .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;

    // Stop overlapping runs from both sending today's email.
    let _lock = match acquire_lock(&state_dir)? {
        Some(lock) => lock,
        None => return Ok(()),
    };

    let memory = read_memory(&meminfo_path())?;
    log_sample(&memory, &state_dir)?;

    let threshold = threshold()?;
    if !is_over_threshold(&memory, threshold) {
        return Ok(());
    }

    let today = format_time("%F");
    let last_sent = fs::read_to_string(state_dir.join("last_sent")).unwrap_or_default();
    if last_sent.trim_end_matches('\n') == today {
        return Ok(());
    }

    let email = compose_email(&memory, &state_dir, false, sender_name.as_deref())?;
    let message = render_message(&email, &recipient, &sender, sender_name.as_deref());

    let sent = match find_mailer() {
        Some(mailer) => send_email(&mailer, &email, &recipient, &message),
        None => Err("no sendmail, msmtp or mail command found".to_string()),
    };
    if let Err(e) = sent {
        eprintln!("ram-altman: {e}");
        return Err("email failed to send; will try again on the next run".to_string());
    }

    // Recorded only after a successful send, so a failure gets retried.
    fs::write(state_dir.join("last_sent"), format!("{today}\n"))
        .map_err(|e| format!("cannot record last send: {e}"))?;

    Ok(())
}

fn dry_run() -> Result<(), String> {
    let memory = read_memory(&meminfo_path())?;
    let threshold = threshold()?;
    print_status(&memory);

    if !is_over_threshold(&memory, threshold) {
        println!("No email: available RAM is not above {threshold}%.");
        return Ok(());
    }

    // A dry run changes nothing on disk, so the bag of jokes is left alone.
    let state_dir = state_dir_path().unwrap_or_else(|_| PathBuf::from("."));
    let sender_name = nonempty_env("RAM_ALTMAN_FROM_NAME");
    let email = compose_email(&memory, &state_dir, true, sender_name.as_deref())?;

    println!();
    print!(
        "{}",
        render_message(
            &email,
            "recipient-not-set@example.com",
            "sender-not-set@example.com",
            sender_name.as_deref(),
        )
    );

    Ok(())
}

// ----- Settings --------------------------------------------------------------

fn nonempty_env(name: &str) -> Option<String> {
    env::var_os(name)
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
}

fn meminfo_path() -> PathBuf {
    nonempty_env("RAM_ALTMAN_MEMINFO")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/proc/meminfo"))
}

fn threshold() -> Result<u64, String> {
    match nonempty_env("RAM_ALTMAN_THRESHOLD") {
        None => Ok(10),
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| format!("RAM_ALTMAN_THRESHOLD is not a whole number: {value}")),
    }
}

fn state_dir_path() -> Result<PathBuf, String> {
    if let Some(dir) = nonempty_env("RAM_ALTMAN_STATE_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(dir) = nonempty_env("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("ram-altman"));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".local/state/ram-altman"))
}

// ----- Memory ----------------------------------------------------------------

#[derive(Debug)]
struct Memory {
    total_kb: u64,
    available_kb: u64,
    used_kb: u64,
    free_kb: u64,
    cache_kb: u64,
}

fn read_memory(path: &Path) -> Result<Memory, String> {
    let text = fs::read_to_string(path)
        .map_err(|_| format!("cannot read {} (this program needs Linux)", path.display()))?;

    let mut total = 0u64;
    let mut free = 0u64;
    let mut available = 0u64;
    let mut has_available = false;
    let mut buffers = 0u64;
    let mut cached = 0u64;
    let mut slab = 0u64;
    let mut shmem = 0u64;

    for line in text.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|number| number.parse::<u64>().ok())
            .unwrap_or(0);
        match key {
            "MemTotal" => total = value,
            "MemFree" => free = value,
            "MemAvailable" => {
                available = value;
                has_available = true;
            }
            "Buffers" => buffers = value,
            "Cached" => cached = value,
            "SReclaimable" => slab = value,
            "Shmem" => shmem = value,
            _ => {}
        }
    }

    if total == 0 {
        return Err(format!("could not find MemTotal in {}", path.display()));
    }

    let cache = buffers
        .saturating_add(cached)
        .saturating_add(slab)
        .saturating_sub(shmem);

    // Kernels older than 3.14 have no MemAvailable, so estimate it.
    if !has_available {
        available = free.saturating_add(cache);
    }
    if available > total {
        available = total;
    }

    Ok(Memory {
        total_kb: total,
        available_kb: available,
        used_kb: total - available,
        free_kb: free,
        cache_kb: cache,
    })
}

fn gib(kilobytes: u64) -> String {
    format!("{:.1} GiB", kilobytes as f64 / 1_048_576.0)
}

fn percentage(part: u64, whole: u64) -> String {
    format!("{:.1}", part as f64 * 100.0 / whole as f64)
}

fn is_over_threshold(memory: &Memory, threshold: u64) -> bool {
    memory.available_kb as u128 * 100 > threshold as u128 * memory.total_kb as u128
}

fn print_status(memory: &Memory) {
    println!("Total:      {}", gib(memory.total_kb));
    println!("Used:       {}", gib(memory.used_kb));
    println!(
        "Available:  {} ({}%)",
        gib(memory.available_kb),
        percentage(memory.available_kb, memory.total_kb)
    );
    println!("  Free:     {}", gib(memory.free_kb));
    println!("  Cache:    {}", gib(memory.cache_kb));
}

// ----- CSV log ---------------------------------------------------------------

fn log_sample(memory: &Memory, state_dir: &Path) -> Result<(), String> {
    let path = state_dir.join("usage.csv");

    if !path.exists() {
        fs::write(
            &path,
            "timestamp,total_kb,used_kb,available_kb,available_pct\n",
        )
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    }

    let line = format!(
        "{},{},{},{},{}\n",
        format_time("%Y-%m-%dT%H:%M:%S%z"),
        memory.total_kb,
        memory.used_kb,
        memory.available_kb,
        percentage(memory.available_kb, memory.total_kb),
    );

    let mut file = OpenOptions::new()
        .append(true)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    drop(file);

    let size = fs::metadata(&path)
        .map_err(|e| format!("cannot stat {}: {e}", path.display()))?
        .len();
    if size > MAX_LOG_BYTES {
        trim_log(&path)?;
    }

    Ok(())
}

/// Keep the header and the most recent lines once the log gets too big.
fn trim_log(path: &Path) -> Result<(), String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;

    let mut lines = text.lines();
    let header = lines
        .next()
        .unwrap_or("timestamp,total_kb,used_kb,available_kb,available_pct");
    let rows: Vec<&str> = lines.collect();
    let keep_from = rows.len().saturating_sub(LOG_LINES_TO_KEEP);

    let mut trimmed = String::with_capacity(text.len().min(MAX_LOG_BYTES as usize));
    trimmed.push_str(header);
    trimmed.push('\n');
    for row in &rows[keep_from..] {
        trimmed.push_str(row);
        trimmed.push('\n');
    }

    let temporary = path.with_extension("csv.tmp");
    fs::write(&temporary, trimmed)
        .map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    fs::rename(&temporary, path).map_err(|e| format!("cannot replace {}: {e}", path.display()))?;

    Ok(())
}

// ----- Joke selection --------------------------------------------------------

/// Deals out every option once, in random order, before any can come up again.
/// Prints an index from 0 to count-1. A dry run changes nothing on disk.
fn pick(name: &str, count: usize, state_dir: &Path, dry_run: bool) -> Result<usize, String> {
    if count == 0 {
        return Ok(0);
    }
    if dry_run {
        return Ok((random_u64() % count as u64) as usize);
    }

    let bag_path = state_dir.join(format!("bag_{name}"));
    let last_path = state_dir.join(format!("bag_{name}.last"));

    let needs_filling = match fs::metadata(&bag_path) {
        Ok(metadata) => metadata.len() == 0,
        Err(_) => true,
    };

    if needs_filling {
        let mut items: Vec<usize> = (0..count).collect();
        let mut rng = Rng::new();
        shuffle(&mut items, &mut rng);

        // Stop the last pick of the old round opening the new one.
        let last = fs::read_to_string(&last_path).ok().and_then(|text| {
            text.lines()
                .next()
                .and_then(|l| l.trim().parse::<usize>().ok())
        });
        if count > 1 && items.first().copied() == last {
            items.rotate_left(1);
        }

        let bag: String = items.iter().map(|item| format!("{item}\n")).collect();
        fs::write(&bag_path, bag)
            .map_err(|e| format!("cannot write {}: {e}", bag_path.display()))?;
    }

    let contents = fs::read_to_string(&bag_path)
        .map_err(|e| format!("cannot read {}: {e}", bag_path.display()))?;
    let mut lines: Vec<&str> = contents.lines().collect();
    let next = if lines.is_empty() {
        0
    } else {
        lines.remove(0).trim().parse::<usize>().unwrap_or(0)
    };

    let rest: String = lines.iter().map(|line| format!("{line}\n")).collect();
    fs::write(&bag_path, rest).map_err(|e| format!("cannot write {}: {e}", bag_path.display()))?;
    fs::write(&last_path, format!("{next}\n"))
        .map_err(|e| format!("cannot write {}: {e}", last_path.display()))?;

    // Covers the template lists being shortened after the bag was dealt.
    if next < count {
        Ok(next)
    } else {
        Ok((random_u64() % count as u64) as usize)
    }
}

struct Rng(u64);

impl Rng {
    fn new() -> Rng {
        let mut bytes = [0u8; 8];
        if let Ok(mut file) = File::open("/dev/urandom") {
            if file.read_exact(&mut bytes).is_ok() {
                return Rng(u64::from_le_bytes(bytes));
            }
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0);
        Rng(nanos ^ ((std::process::id() as u64) << 32) ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
}

fn random_u64() -> u64 {
    Rng::new().next()
}

fn shuffle(items: &mut [usize], rng: &mut Rng) {
    for i in (1..items.len()).rev() {
        let j = (rng.next() % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

// ----- The correspondence ----------------------------------------------------

struct Email {
    subject: String,
    body: String,
}

fn compose_email(
    memory: &Memory,
    state_dir: &Path,
    dry_run: bool,
    sender_name: Option<&str>,
) -> Result<Email, String> {
    let total = gib(memory.total_kb);
    let available = gib(memory.available_kb);
    let pct = percentage(memory.available_kb, memory.total_kb);

    let subjects = [
        "Good news: we found some RAM",
        "Unused RAM detected in the wild",
        "RAM sighting on my PC",
        "$available of RAM, just sitting there",
        "You missed a spot",
        "Daily RAM census: some survived",
        "$pct% of my RAM remains at large",
        "Still not all of it, I'm afraid",
    ];

    let bodies = [
        "I'm writing to report that you appear to have missed some. This PC has $total of RAM and $available of it ($pct%) is currently available. I assume this is an oversight.",
        "A routine inspection of my PC found $available of unclaimed RAM out of $total. That's $pct% of the machine doing absolutely nothing. I thought you'd want to know before someone tries to train something on it.",
        "Daily status report:\n\n  Total RAM:     $total\n  Available:     $available ($pct%)\n  Threat level:  someone at OpenAI might notice",
        "Even with every browser tab I own open, $available of my $total of RAM is still available. I'm as surprised as you are.",
        "Please don't be alarmed, but there is $available of RAM on my desk that isn't in a data centre. That's $pct% of what's installed here, and as far as I can tell it has no plans today.",
        "Before you ask: yes, I counted the page cache as available. The kernel gives it back the moment anyone asks, which leaves $available of $total up for grabs.",
        "Today's forecast for my PC: clear skies, light workloads and $pct% available memory. Expect $available of the installed $total to stay idle through the evening.",
        "I hear RAM is hard to come by these days, so it feels a bit rude to be sitting on $available of it. Total installed: $total. It's not for sale, I just like knowing it's there.",
        "I have a confession. I have $available of RAM that I'm not using, which is $pct% of the $total in this PC. I've told nobody but you and a cron job.",
        "If $available of RAM sits idle in a PC and nobody allocates it, does it still count as capacity? Asking on behalf of the $pct% of my memory with nothing to do.",
    ];

    let signoffs = [
        "Yours in abundance,",
        "Regards from the last free RAM in the county,",
        "Hoarding responsibly,",
        "Stay allocated,",
        "Yours, with memory to spare,",
        "Kind regards,",
    ];

    let subject = subjects[pick("subject", subjects.len(), state_dir, dry_run)?];
    let body = bodies[pick("body", bodies.len(), state_dir, dry_run)?];
    let signoff = signoffs[pick("signoff", signoffs.len(), state_dir, dry_run)?];

    let fill = |text: &str| {
        text.replace("$total", &total)
            .replace("$available", &available)
            .replace("$pct", &pct)
    };

    let signature = sender_name.unwrap_or("A concerned RAM owner");

    let body = format!(
        "Hi Sam,\n\n{}\n\n{}\n{}\n\n--\nSent automatically, at most once a day, by a small script on one PC.\nReply if you'd like it to stop.",
        fill(body),
        signoff,
        signature
    );

    Ok(Email {
        subject: fill(subject),
        body,
    })
}

fn render_message(
    email: &Email,
    recipient: &str,
    sender: &str,
    sender_name: Option<&str>,
) -> String {
    let from = match sender_name {
        Some(name) => format!("{name} <{sender}>"),
        None => sender.to_string(),
    };

    format!(
        "From: {from}\nTo: {recipient}\nSubject: {}\nDate: {}\nMIME-Version: 1.0\nContent-Type: text/plain; charset=UTF-8\nAuto-Submitted: auto-generated\n\n{}\n",
        email.subject,
        format_time("%a, %d %b %Y %H:%M:%S %z"),
        email.body
    )
}

// ----- Sending ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum MailerKind {
    Sendmail,
    Msmtp,
    Mail,
}

struct Mailer {
    kind: MailerKind,
    path: PathBuf,
}

fn find_mailer() -> Option<Mailer> {
    for (name, kind) in [
        ("sendmail", MailerKind::Sendmail),
        ("msmtp", MailerKind::Msmtp),
        ("mail", MailerKind::Mail),
    ] {
        if let Some(path) = find_in_path(name) {
            return Some(Mailer { kind, path });
        }
    }
    None
}

/// cron's PATH often leaves out the directories sendmail lives in.
fn find_in_path(name: &str) -> Option<PathBuf> {
    let mut directories: Vec<PathBuf> = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect())
        .unwrap_or_default();
    directories.push(PathBuf::from("/usr/sbin"));
    directories.push(PathBuf::from("/sbin"));

    directories
        .into_iter()
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn send_email(
    mailer: &Mailer,
    email: &Email,
    recipient: &str,
    message: &str,
) -> Result<(), String> {
    let mut command = Command::new(&mailer.path);
    match mailer.kind {
        MailerKind::Sendmail => {
            command.args(["-t", "-i"]);
        }
        MailerKind::Msmtp => {
            command.arg("-t");
        }
        MailerKind::Mail => {
            command.arg("-s").arg(&email.subject).arg(recipient);
        }
    }

    command.stdin(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", mailer.path.display()))?;

    let payload = match mailer.kind {
        MailerKind::Mail => format!("{}\n", email.body),
        _ => message.to_string(),
    };

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .map_err(|e| format!("cannot write to the mailer: {e}"))?;
    }

    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for the mailer: {e}"))?;
    if !status.success() {
        return Err(match status.code() {
            Some(code) => format!("mailer exited with status {code}"),
            None => "mailer was killed by a signal".to_string(),
        });
    }

    Ok(())
}

// ----- Locking ---------------------------------------------------------------

fn acquire_lock(state_dir: &Path) -> Result<Option<File>, String> {
    let path = state_dir.join("lock");
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    const LOCK_EX: c_int = 2;
    const LOCK_NB: c_int = 4;
    let locked = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0;
    Ok(if locked { Some(file) } else { None })
}

// ----- Time ------------------------------------------------------------------

type TimeT = i64;

#[repr(C)]
struct Tm {
    tm_sec: c_int,
    tm_min: c_int,
    tm_hour: c_int,
    tm_mday: c_int,
    tm_mon: c_int,
    tm_year: c_int,
    tm_wday: c_int,
    tm_yday: c_int,
    tm_isdst: c_int,
    tm_gmtoff: c_long,
    tm_zone: *const c_char,
}

extern "C" {
    fn flock(fd: c_int, operation: c_int) -> c_int;
    fn tzset();
    fn localtime_r(timep: *const TimeT, result: *mut Tm) -> *mut Tm;
    fn strftime(buffer: *mut c_char, max: usize, format: *const c_char, tm: *const Tm) -> usize;
}

fn format_time(format: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as TimeT)
        .unwrap_or(0);
    let format = CString::new(format).expect("time format has no NUL bytes");
    let mut tm: Tm = unsafe { std::mem::zeroed() };
    let mut buffer = [0 as c_char; 128];

    let length = unsafe {
        if localtime_r(&now, &mut tm).is_null() {
            0
        } else {
            strftime(buffer.as_mut_ptr(), buffer.len(), format.as_ptr(), &tm)
        }
    };

    let bytes: Vec<u8> = buffer[..length].iter().map(|&byte| byte as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}
