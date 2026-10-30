//! Finding, starting and talking to the background heater.
//!
//! There is at most one heater per user. It holds an exclusive OS lock on
//! `warm-<user>.lock` for its whole life (released by the OS even if it
//! crashes, so there are no stale locks) and publishes its localhost port and
//! a secret token in `warm-<user>.json`. Requests are one line each:
//! `<token> <command>`, answered with one line.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::hash::{BuildHasher, RandomState};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

use crate::Opts;

#[derive(Serialize, Deserialize)]
pub struct Info {
    pub pid: u32,
    pub port: u16,
    pub token: String,
}

/// What the heater reports once a second.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Status {
    pub sensor: String,
    pub uptime_s: u64,
    pub temp: Option<f32>,
    pub target: f32,
    pub state: String,
    pub duty: f32,
    pub max_duty: f32,
    pub cpu_pct: f32,
    pub ours_pct: f32,
    pub cpu_c: Option<f32>,
    pub battery_c: Option<f32>,
    pub charge_pct: Option<f32>,
    pub on_battery: bool,
    pub watts: Option<f32>,
    /// GPU heater output 0..=1, or None if there's no GPU heater.
    pub gpu: Option<f32>,
    /// Controlled temperature, one sample per second, oldest first.
    pub history: Vec<Option<f32>>,
}

fn path(ext: &str) -> PathBuf {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into());
    std::env::temp_dir().join(format!("warm-{user}.{ext}"))
}

fn open_lock() -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path("lock"))
}

/// Take the single-heater lock, or None if another heater holds it.
/// Retries briefly: a client probing the lock holds it for a moment.
pub fn acquire_lock() -> Option<File> {
    let file = open_lock().ok()?;
    let until = Instant::now() + Duration::from_millis(500);
    loop {
        match file.try_lock() {
            Ok(()) => return Some(file),
            Err(TryLockError::WouldBlock) if Instant::now() < until => {
                thread::sleep(Duration::from_millis(20))
            }
            Err(_) => return None,
        }
    }
}

/// The running heater, if any. The lock, not the info file, is the truth:
/// a leftover info file from a crash is ignored.
pub fn find() -> Option<Info> {
    let file = open_lock().ok()?;
    match file.try_lock() {
        Ok(()) => None, // nobody holds it; dropping the file releases it
        Err(TryLockError::WouldBlock) => {
            let text = fs::read_to_string(path("json")).ok()?;
            serde_json::from_str(&text).ok()
        }
        Err(_) => None,
    }
}

pub fn publish(info: &Info) -> io::Result<()> {
    let tmp = path("json.tmp");
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600); // the token is a credential
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(serde_json::to_string(info)?.as_bytes())?;
    fs::rename(tmp, path("json"))
}

pub fn unpublish() {
    let _ = fs::remove_file(path("json"));
}

pub fn new_token() -> String {
    // RandomState is seeded from the OS RNG; good enough for a local secret
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let a = RandomState::new().hash_one(nanos);
    let b = RandomState::new().hash_one(std::process::id());
    format!("{a:016x}{b:016x}")
}

pub fn request(info: &Info, cmd: &str) -> io::Result<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], info.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(1))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    writeln!(stream, "{} {cmd}", info.token)?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    match line.trim_end() {
        "" => Err(io::Error::other("heater closed the connection")),
        reply => Ok(reply.to_string()),
    }
}

pub fn status(info: &Info) -> io::Result<Status> {
    let reply = request(info, "status")?;
    serde_json::from_str(&reply).map_err(io::Error::other)
}

/// Send any target/cap given on the command line to a running heater.
pub fn apply(info: &Info, opts: &Opts) {
    if let Some(t) = opts.target {
        let _ = request(info, &format!("target {t}"));
    }
    if let Some(m) = opts.max_duty {
        let _ = request(info, &format!("cap {}", m / 100.0));
    }
}

/// Start the heater detached from this terminal and wait until it answers.
pub fn spawn(opts: &Opts) -> io::Result<Info> {
    let log = File::create(path("log"))?;
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("daemon")
        .args(opts.to_args())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log);
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        // own session: no controlling terminal, so closing it doesn't kill us
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn()?;

    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if let Some(info) = find()
            && status(&info).is_ok()
        {
            return Ok(info);
        }
        thread::sleep(Duration::from_millis(50));
    }
    let why = fs::read_to_string(path("log")).unwrap_or_default();
    Err(io::Error::other(match why.trim() {
        "" => "heater didn't start".to_string(),
        why => why.to_string(),
    }))
}

/// Ask the heater to stop and wait for it to let go of the lock.
pub fn stop(info: &Info) -> bool {
    let _ = request(info, "stop");
    let until = Instant::now() + Duration::from_secs(5);
    while Instant::now() < until {
        if find().is_none() {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}
