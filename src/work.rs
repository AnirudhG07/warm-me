//! Useful heat. Instead of warm's own pointless math, real volunteer
//! computing can make the heat: a watt spent finding primes or folding
//! proteins warms the laptop exactly as much as a wasted one.
//!
//! - any program (`--pid`, `--run`) and GIMPS (`mprime`) are throttled
//!   directly: paused and resumed many times a second (SIGSTOP/SIGCONT on
//!   Unix, a job-object CPU cap on Windows), which gives fine control.
//! - BOINC and Folding@home run as background services with their own
//!   controls, so they're switched on and off through those (`boinccmd`, and
//!   Folding@home's local websocket), at most every 30 s, and put back the
//!   way they were when warm stops.
//!
//! warm's own heater tops up whatever the project leaves idle, and takes
//! over completely if the project stops.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::ValueEnum;
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

/// Volunteer-computing projects warm knows how to drive.
#[derive(Clone, Copy, PartialEq, ValueEnum)]
pub enum Project {
    /// GIMPS: hunt for record Mersenne primes (mersenne.org, runs mprime)
    Primes,
    /// BOINC: Rosetta@home, Einstein@home, World Community Grid and more
    Boinc,
    /// Folding@home: protein simulations for disease research
    Folding,
}

/// What makes the heat.
#[derive(Clone)]
pub enum Spec {
    Builtin,
    Pid(u32),
    Run(String),
    Project(Project),
}

/// Switch on/off services no more often than this: their work units take a
/// while to spin up, and flapping helps nobody.
const MIN_SWITCH: Duration = Duration::from_secs(30);
/// On/off services: on above this share of full heat, off below the next.
const SWITCH_ON: f32 = 0.5;
const SWITCH_OFF: f32 = 0.2;

pub struct Work {
    pub label: String,
    /// Root of the process tree whose CPU counts as the project's.
    pub root: u32,
    ctl: Ctl,
}

enum Ctl {
    Throttle {
        throttle: Throttle,
        /// Set when warm started the program, so it stops it again.
        child: Option<Child>,
    },
    Switch {
        service: Service,
        on: Option<bool>,
        last: Option<Instant>,
    },
}

impl Work {
    /// Find and start the heat source. Errors are meant for the user.
    pub fn start(spec: &Spec) -> Result<Option<Work>, String> {
        match spec {
            Spec::Builtin => Ok(None),
            Spec::Pid(pid) => attach(*pid).map(Some),
            Spec::Run(cmd) => run(cmd, &shell(cmd), None, cmd.clone()).map(Some),
            Spec::Project(Project::Primes) => {
                let (exe, dir) = find_mprime()?;
                let mut cmd = Command::new(&exe);
                if cfg!(unix) {
                    cmd.arg("-d"); // run in the foreground, no menu
                }
                let label = "GIMPS prime search".to_string();
                run(&exe.display().to_string(), &cmd, Some(&dir), label).map(Some)
            }
            Spec::Project(Project::Boinc) => switched(Service::boinc()?).map(Some),
            Spec::Project(Project::Folding) => switched(Service::folding()?).map(Some),
        }
    }

    /// Share of its full output the project should run at, 0..=1.
    pub fn set(&mut self, share: f32, tree: &[u32]) {
        match &mut self.ctl {
            Ctl::Throttle { throttle, .. } => throttle.set(share, tree),
            Ctl::Switch { service, on, last } => {
                let want = match *on {
                    Some(true) => share > SWITCH_OFF,
                    _ => share >= SWITCH_ON,
                };
                let due = last.is_none_or(|t| t.elapsed() >= MIN_SWITCH);
                if Some(want) != *on && due {
                    if let Err(e) = service.switch(want) {
                        eprintln!("{}: {e}", service.name);
                    }
                    (*on, *last) = (Some(want), Some(Instant::now()));
                }
            }
        }
    }

    /// Whether warm should still count on this source. Services also count
    /// as gone if they're switched on but their controls stopped answering.
    pub fn alive(&mut self) -> bool {
        match &mut self.ctl {
            Ctl::Throttle { child: Some(c), .. } => matches!(c.try_wait(), Ok(None)),
            Ctl::Throttle { .. } => process_exists(self.root),
            Ctl::Switch { service, .. } => process_exists(self.root) && service.ok,
        }
    }

    /// Hand everything back: resume a paused program (stopping it if warm
    /// started it), or put a service back the way it was.
    pub fn release(self) {
        match self.ctl {
            Ctl::Throttle {
                throttle,
                mut child,
            } => {
                let tree = throttle.release();
                if let Some(c) = &mut child {
                    stop_child(c, self.root, &tree);
                }
            }
            Ctl::Switch { mut service, .. } => service.restore(),
        }
    }
}

fn attach(pid: u32) -> Result<Work, String> {
    if pid <= 1 || pid == std::process::id() {
        return Err(format!("won't take over process {pid}"));
    }
    let name = process_name(pid).ok_or(format!("no process {pid} is running"))?;
    let throttle = Throttle::new(pid)?;
    Ok(Work {
        label: format!("{name} (pid {pid})"),
        root: pid,
        ctl: Ctl::Throttle {
            throttle,
            child: None,
        },
    })
}

fn run(what: &str, cmd: &Command, dir: Option<&Path>, label: String) -> Result<Work, String> {
    let mut cmd = rebuild(cmd);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("couldn't start {what}: {e}"))?;
    let root = child.id();
    let throttle = Throttle::new(root)?;
    Ok(Work {
        label,
        root,
        ctl: Ctl::Throttle {
            throttle,
            child: Some(child),
        },
    })
}

fn switched(service: Service) -> Result<Work, String> {
    Ok(Work {
        label: service.name.to_string(),
        root: service.root,
        ctl: Ctl::Switch {
            service,
            on: None,
            last: None,
        },
    })
}

fn shell(line: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", line]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", line]);
        c
    }
}

/// Command isn't Clone; copy program and args.
fn rebuild(cmd: &Command) -> Command {
    let mut c = Command::new(cmd.get_program());
    c.args(cmd.get_args());
    c
}

/// Politely stop a program warm started: let it save its work first.
fn stop_child(child: &mut Child, root: u32, tree: &[u32]) {
    #[cfg(unix)]
    unsafe {
        for &p in tree.iter().chain([&root]) {
            libc::kill(p as i32, libc::SIGCONT);
            libc::kill(p as i32, libc::SIGTERM);
        }
    }
    #[cfg(not(unix))]
    let _ = (root, tree);
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if !matches!(child.try_wait(), Ok(None)) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ---------------------------------------------------------------------------
// Throttling a program directly

/// Runs the program for `share` of every 100 ms and pauses it for the rest.
struct Throttle {
    duty: Arc<AtomicU32>,
    pids: Arc<Mutex<Vec<u32>>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    #[cfg(windows)]
    job: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(unix)]
const PERIOD: Duration = Duration::from_millis(100);
const DUTY_SCALE: f32 = 10_000.0;

impl Throttle {
    #[cfg(unix)]
    fn new(root: u32) -> Result<Self, String> {
        // only our own processes can be paused; check before relying on it
        if unsafe { libc::kill(root as i32, 0) } != 0 {
            return Err(format!(
                "can't control process {root}: {}",
                io::Error::last_os_error()
            ));
        }
        let duty = Arc::new(AtomicU32::new(DUTY_SCALE as u32));
        let pids = Arc::new(Mutex::new(vec![root]));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (duty, pids, stop) = (duty.clone(), pids.clone(), stop.clone());
            thread::Builder::new()
                .name("warm-me-throttle".into())
                .spawn(move || pace(&duty, &pids, &stop))
                .map_err(|e| e.to_string())?
        };
        Ok(Throttle {
            duty,
            pids,
            stop,
            worker: Some(worker),
        })
    }

    #[cfg(windows)]
    fn new(root: u32) -> Result<Self, String> {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::*;
        use windows_sys::Win32::System::Threading::*;
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            let proc = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, root);
            let ok = !job.is_null()
                && !proc.is_null()
                && AssignProcessToJobObject(job, proc) != 0;
            if !proc.is_null() {
                CloseHandle(proc);
            }
            if !ok {
                if !job.is_null() {
                    CloseHandle(job);
                }
                return Err(format!(
                    "can't control process {root}: {}",
                    io::Error::last_os_error()
                ));
            }
            Ok(Throttle {
                duty: Arc::new(AtomicU32::new(DUTY_SCALE as u32)),
                pids: Arc::new(Mutex::new(vec![root])),
                stop: Arc::new(AtomicBool::new(false)),
                worker: None,
                job,
            })
        }
    }

    fn set(&self, share: f32, tree: &[u32]) {
        let d = (share.clamp(0.0, 1.0) * DUTY_SCALE) as u32;
        let old = self.duty.swap(d, Ordering::Relaxed);
        *self.pids.lock().unwrap() = tree.to_vec();
        #[cfg(windows)]
        if old.abs_diff(d) >= 100 {
            self.cap(Some(d));
        }
        #[cfg(not(windows))]
        let _ = old;
    }

    /// Windows: a hard CPU cap on the job, in 1/100ths of a percent.
    #[cfg(windows)]
    fn cap(&self, rate: Option<u32>) {
        use windows_sys::Win32::System::JobObjects::*;
        let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
        if let Some(r) = rate {
            info.ControlFlags =
                JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            info.Anonymous.CpuRate = r.clamp(1, 10_000);
        }
        unsafe {
            SetInformationJobObject(
                self.job,
                JobObjectCpuRateControlInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            );
        }
    }

    /// Stop pacing and leave everything running. Returns the last tree.
    fn release(mut self) -> Vec<u32> {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        #[cfg(windows)]
        unsafe {
            self.cap(None);
            windows_sys::Win32::Foundation::CloseHandle(self.job);
        }
        self.pids.lock().unwrap().clone()
    }
}

#[cfg(unix)]
fn pace(duty: &AtomicU32, pids: &Mutex<Vec<u32>>, stop: &AtomicBool) {
    let signal = |sig| {
        for &p in pids.lock().unwrap().iter() {
            unsafe { libc::kill(p as i32, sig) };
        }
    };
    while !stop.load(Ordering::Relaxed) {
        let d = duty.load(Ordering::Relaxed) as f32 / DUTY_SCALE;
        if d >= 0.98 {
            signal(libc::SIGCONT);
            thread::sleep(PERIOD);
        } else if d <= 0.02 {
            signal(libc::SIGSTOP);
            thread::sleep(PERIOD);
        } else {
            signal(libc::SIGCONT);
            thread::sleep(PERIOD.mul_f32(d));
            signal(libc::SIGSTOP);
            thread::sleep(PERIOD.mul_f32(1.0 - d));
        }
    }
    // never leave anything paused
    signal(libc::SIGCONT);
}

// ---------------------------------------------------------------------------
// Services with their own controls

struct Service {
    name: &'static str,
    root: u32,
    kind: Kind,
    /// Whether the last command worked.
    ok: bool,
}

enum Kind {
    Boinc {
        boinccmd: PathBuf,
        dir: Option<PathBuf>,
        /// Run mode to put back: always / auto / never.
        restore: String,
    },
    Folding {
        restore_paused: bool,
    },
}

impl Service {
    fn boinc() -> Result<Self, String> {
        let root = find_process(&["boinc", "boinc_client", "boinc.exe"]).ok_or(
            "BOINC isn't running. Get it from https://boinc.berkeley.edu, pick a project \
             in BOINC Manager, then try again",
        )?;
        let boinccmd = find_program(
            "boinccmd",
            &[
                "/Applications/BOINCManager.app/Contents/Resources/boinccmd",
                "/Library/Application Support/BOINC Data/boinccmd",
                r"C:\Program Files\BOINC\boinccmd.exe",
            ],
        )
        .ok_or("BOINC is running but its boinccmd tool wasn't found")?;
        // boinccmd reads the client's password from its data folder
        let dir = [
            "/Library/Application Support/BOINC Data",
            "/var/lib/boinc-client",
            "/var/lib/boinc",
            r"C:\ProgramData\BOINC",
        ]
        .iter()
        .map(PathBuf::from)
        .find(|d| d.is_dir());
        let mut s = Service {
            name: "BOINC",
            root,
            kind: Kind::Boinc {
                boinccmd,
                dir,
                restore: "auto".into(),
            },
            ok: true,
        };
        let status = s.boinccmd(&["--get_cc_status"])?;
        if let Kind::Boinc { restore, .. } = &mut s.kind {
            *restore = boinc_mode(&status);
        }
        Ok(s)
    }

    fn folding() -> Result<Self, String> {
        let root = find_process(&["fah-client", "fah-client.exe", "FAHClient.exe"]).ok_or(
            "Folding@home isn't running. Get it from https://foldingathome.org/start-folding, \
             sign in, then try again",
        )?;
        let state = fah(None).map_err(|e| {
            format!("Folding@home is running but didn't answer on localhost:7396 ({e})")
        })?;
        Ok(Service {
            name: "Folding@home",
            root,
            kind: Kind::Folding {
                restore_paused: fah_paused(&state),
            },
            ok: true,
        })
    }

    fn switch(&mut self, on: bool) -> Result<(), String> {
        let result = match &self.kind {
            Kind::Boinc { .. } => self
                .boinccmd(&["--set_run_mode", if on { "always" } else { "never" }])
                .map(drop),
            Kind::Folding { .. } => fah(Some(if on { "fold" } else { "pause" }))
                .map(drop)
                .map_err(|e| e.to_string()),
        };
        self.ok = result.is_ok();
        result
    }

    fn restore(&mut self) {
        let result = match &self.kind {
            Kind::Boinc { restore, .. } => {
                let mode = restore.clone();
                self.boinccmd(&["--set_run_mode", &mode]).map(drop)
            }
            Kind::Folding { restore_paused } => {
                fah(Some(if *restore_paused { "pause" } else { "fold" }))
                    .map(drop)
                    .map_err(|e| e.to_string())
            }
        };
        if let Err(e) = result {
            eprintln!("{}: couldn't restore its settings: {e}", self.name);
        }
    }

    fn boinccmd(&self, args: &[&str]) -> Result<String, String> {
        let Kind::Boinc { boinccmd, dir, .. } = &self.kind else {
            unreachable!()
        };
        let mut cmd = Command::new(boinccmd);
        cmd.args(args);
        if let Some(d) = dir {
            cmd.current_dir(d);
        }
        let out = cmd.output().map_err(|e| format!("boinccmd: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "boinccmd {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

/// The permanent CPU run mode from `boinccmd --get_cc_status`.
fn boinc_mode(status: &str) -> String {
    let cpu = status.split("GPU status").next().unwrap_or(status);
    let mode = cpu
        .lines()
        .find_map(|l| l.trim().strip_prefix("perm mode:"))
        .unwrap_or("")
        .trim();
    match mode {
        "always" => "always",
        "never" => "never",
        _ => "auto", // "according to prefs"
    }
    .into()
}

/// Folding@home v8 is controlled over a local websocket, the same one its
/// web control uses. Connect, read the state it sends, and optionally set
/// fold / pause. Returns the state JSON.
fn fah(state: Option<&str>) -> io::Result<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], 7396));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        s,
        "GET /api/websocket HTTP/1.1\r\nHost: 127.0.0.1:7396\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut reader = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if !line.contains(" 101 ") {
        return Err(io::Error::other(format!("unexpected reply: {}", line.trim())));
    }
    while {
        line.clear();
        reader.read_line(&mut line)? > 2
    } {}
    let first = read_frame(&mut reader)?;
    if let Some(state) = state {
        let msg = serde_json::json!({ "cmd": "state", "state": state }).to_string();
        send_frame(&mut s, msg.as_bytes())?;
    }
    Ok(first)
}

fn send_frame(s: &mut TcpStream, data: &[u8]) -> io::Result<()> {
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = vec![0x81]; // final text frame
    match data.len() {
        n if n < 126 => frame.push(0x80 | n as u8),
        n if n <= u16::MAX as usize => {
            frame.push(0x80 | 126);
            frame.extend((n as u16).to_be_bytes());
        }
        n => {
            frame.push(0x80 | 127);
            frame.extend((n as u64).to_be_bytes());
        }
    }
    frame.extend(mask);
    frame.extend(data.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    s.write_all(&frame)
}

/// One whole text message from the server (unmasked, maybe fragmented).
fn read_frame(r: &mut impl Read) -> io::Result<String> {
    let mut msg = Vec::new();
    loop {
        let mut head = [0u8; 2];
        r.read_exact(&mut head)?;
        let len = match head[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                r.read_exact(&mut b)?;
                u16::from_be_bytes(b) as u64
            }
            127 => {
                let mut b = [0u8; 8];
                r.read_exact(&mut b)?;
                u64::from_be_bytes(b)
            }
            n => n as u64,
        };
        if msg.len() as u64 + len > 16 << 20 {
            return Err(io::Error::other("message too large"));
        }
        let mut data = vec![0u8; len as usize];
        r.read_exact(&mut data)?;
        let opcode = head[0] & 0x0f;
        if opcode >= 0x8 {
            continue; // ping / pong / close: not what we're after
        }
        msg.extend(data);
        if head[0] & 0x80 != 0 {
            return Ok(String::from_utf8_lossy(&msg).into_owned());
        }
    }
}

/// Whether Folding@home was paused: any `"paused": true` in its state.
fn fah_paused(state: &str) -> bool {
    fn find(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => m
                .iter()
                .any(|(k, v)| (k == "paused" && v == &serde_json::Value::Bool(true)) || find(v)),
            serde_json::Value::Array(a) => a.iter().any(find),
            _ => false,
        }
    }
    serde_json::from_str(state).is_ok_and(|v| find(&v))
}

// ---------------------------------------------------------------------------
// Finding things

/// mprime (Prime95 on Windows) and the folder it keeps its work in.
fn find_mprime() -> Result<(PathBuf, PathBuf), String> {
    let name = if cfg!(windows) { "prime95.exe" } else { "mprime" };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default();
    let spots: Vec<PathBuf> = ["mprime", "prime95", "Prime95"]
        .iter()
        .map(|d| home.join(d).join(name))
        .collect();
    let spots: Vec<&str> = spots.iter().filter_map(|p| p.to_str()).collect();
    let exe = find_program(name, &spots).ok_or(format!(
        "GIMPS ({name}) not found. Get it from https://www.mersenne.org/download/, unpack \
         it into ~/mprime, run `{name} -m` there once to join GIMPS, then try again"
    ))?;
    let dir = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    if !dir.join("prime.txt").exists() {
        return Err(format!(
            "GIMPS isn't set up yet: run `{} -m` once in {} and choose to join GIMPS, \
             so your results are credited to you",
            exe.display(),
            dir.display()
        ));
    }
    Ok((exe, dir))
}

fn find_program(name: &str, spots: &[&str]) -> Option<PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join(name))
            .find(|p| p.is_file())
    });
    on_path.or_else(|| spots.iter().map(PathBuf::from).find(|p| p.is_file()))
}

fn find_process(names: &[&str]) -> Option<u32> {
    let mut sys = System::new().ok()?;
    sys.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    sys.processes()
        .values()
        .find(|p| names.iter().any(|n| p.name() == *n))
        .map(|p| p.pid().as_u32())
}

fn process_name(pid: u32) -> Option<String> {
    let mut sys = System::new().ok()?;
    let pid = sysinfo::Pid::from_u32(pid);
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    sys.process(pid)
        .map(|p| p.name().to_string_lossy().into_owned())
}

fn process_exists(pid: u32) -> bool {
    process_name(pid).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_boinc_run_mode() {
        let status = "network connection status: online\nCPU status\n    suspended: no\n    \
                      current mode: never\n    perm mode: always\n    perm becomes current \
                      in 0 sec\nGPU status\n    perm mode: never\n";
        assert_eq!(boinc_mode(status), "always");
        assert_eq!(boinc_mode("CPU status\n    perm mode: according to prefs\n"), "auto");
    }

    #[test]
    fn reads_folding_paused() {
        assert!(fah_paused(r#"{"config":{"paused":true}}"#));
        assert!(fah_paused(r#"{"groups":{"":{"config":{"paused":true}}}}"#));
        assert!(!fah_paused(r#"{"config":{"paused":false}}"#));
    }
}
