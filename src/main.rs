//! warm — keep a laptop warm in the cold by turning spare CPU into heat.
//!
//! One background heater per user does the work (see `daemon`); `warm` opens
//! a small live view of it, `warm on` / `warm off` start and stop it.

mod client;
mod daemon;
#[cfg(feature = "gpu")]
mod gpu;
#[cfg(not(feature = "gpu"))]
mod gpu {
    //! Built without the GPU heater.
    pub struct Gpu {
        pub name: String,
    }
    impl Gpu {
        pub fn start() -> Option<Self> {
            None
        }
        pub fn set_duty(&self, _: f32) {}
    }
}
mod heater;
mod sensors;
mod ui;

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};

use client::{Info, Status};
use sensors::{Reading, Sensors};

/// Battery chemistry limit: don't push the pack itself past this.
const BATTERY_MAX_C: f32 = 40.0;

#[derive(Parser)]
#[command(
    name = "warm",
    version,
    about = "Keep your laptop warm in the cold by turning spare CPU into heat",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    #[command(flatten)]
    opts: Opts,

    /// Print a status line each second instead of the live view
    #[arg(long)]
    plain: bool,

    /// List detected sensors and exit
    #[arg(short, long)]
    list: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start heating in the background
    On(Opts),
    /// Stop the background heater
    Off,
    /// The background heater itself (started by `warm` / `warm on`)
    #[command(hide = true)]
    Daemon(Opts),
}

#[derive(Args, Clone)]
pub struct Opts {
    /// Temperature to hold, in °C [default: 35 for battery, 55 for cpu]
    target: Option<f32>,

    /// Which sensor to regulate on
    #[arg(short, long, value_enum, default_value_t = Source::Auto)]
    sensor: Source,

    /// Worker threads [default: all logical cores]
    #[arg(short, long)]
    threads: Option<usize>,

    /// Power cap, in percent of full heater output [default: 100]
    #[arg(short = 'm', long)]
    max_duty: Option<f32>,

    /// Pause when on battery below this charge, in percent
    #[arg(long, default_value_t = 20.0)]
    min_battery: f32,

    /// Only heat while plugged in (heating on battery costs charge)
    #[arg(short, long)]
    plugged_only: bool,

    /// Back off if the CPU gets hotter than this, in °C. Chips throttle
    /// themselves near 100°C; backing off earlier only wastes heat.
    #[arg(long, default_value_t = 105.0)]
    cpu_limit: f32,

    /// Keep heating at full power even while you're using the CPU
    #[arg(long)]
    no_yield: bool,

    /// Don't raise the heater's priority while warming up from cold
    #[arg(long)]
    no_boost: bool,

    /// Don't use the GPU as a second heater while warming up
    #[arg(long)]
    no_gpu: bool,
}

impl Opts {
    /// The same options as command-line arguments, for spawning the heater.
    fn to_args(&self) -> Vec<String> {
        let mut a = Vec::new();
        if let Some(t) = self.target {
            a.push(t.to_string());
        }
        a.extend(["--sensor".into(), name(self.sensor).into()]);
        if let Some(n) = self.threads {
            a.extend(["--threads".into(), n.to_string()]);
        }
        if let Some(m) = self.max_duty {
            a.extend(["--max-duty".into(), m.to_string()]);
        }
        a.extend(["--min-battery".into(), self.min_battery.to_string()]);
        a.extend(["--cpu-limit".into(), self.cpu_limit.to_string()]);
        if self.plugged_only {
            a.push("--plugged-only".into());
        }
        if self.no_yield {
            a.push("--no-yield".into());
        }
        if self.no_boost {
            a.push("--no-boost".into());
        }
        if self.no_gpu {
            a.push("--no-gpu".into());
        }
        a
    }
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
pub enum Source {
    /// Battery if it reports a temperature, else CPU
    Auto,
    /// Battery pack temperature: best proxy for how warm the laptop feels
    Battery,
    /// Hottest CPU/SoC die sensor: reacts fast, but runs far above chassis temp
    Cpu,
}

fn main() {
    let cli = Cli::parse();
    match cli.cmd {
        Some(Cmd::Daemon(opts)) => daemon::run(opts),
        Some(Cmd::On(opts)) => on(&opts),
        Some(Cmd::Off) => off(),
        None if cli.list => list(),
        None => open(&cli.opts, cli.plain),
    }
}

/// `warm`: attach to the heater (starting it if needed) and watch it.
fn open(opts: &Opts, plain: bool) {
    let info = connect_or_start(opts);
    if plain || !io::stdout().is_terminal() {
        return watch_plain(&info);
    }
    match ui::run(&info) {
        Ok(ui::Exit::Background) => {
            println!("warm keeps heating in the background · `warm` to reopen · `warm off` to stop")
        }
        Ok(ui::Exit::Quit) => println!("warm stopped"),
        Ok(ui::Exit::Gone) => println!("warm stopped (turned off elsewhere)"),
        Err(e) => fail(&format!("terminal error: {e}")),
    }
}

/// `warm on`: make sure the heater is running, then return.
fn on(opts: &Opts) {
    if let Some(info) = client::find() {
        client::apply(&info, opts);
        println!("warm is already on");
        return;
    }
    let info = connect_or_start(opts);
    let target = client::status(&info).map_or(String::new(), |s| {
        format!(" to {:.1}°C ({} sensor)", s.target, s.sensor)
    });
    println!("warm on: heating{target} in the background · `warm` to watch · `warm off` to stop");
}

/// `warm off`: stop the heater if it's running.
fn off() {
    match client::find() {
        None => println!("warm is already off"),
        Some(info) if client::stop(&info) => println!("warm off"),
        Some(info) => fail(&format!(
            "warm didn't stop; it's process {}, you may need to kill it",
            info.pid
        )),
    }
}

fn connect_or_start(opts: &Opts) -> Info {
    if let Some(info) = client::find() {
        client::apply(&info, opts);
        return info;
    }
    // check here too, so a missing sensor is reported in this terminal
    let first = Sensors::new().read();
    if resolve_source(opts.sensor, &first).is_none() {
        fail(&format!(
            "no {} temperature sensor found; try `warm --list`",
            name(opts.sensor)
        ));
    }
    client::spawn(opts).unwrap_or_else(|e| fail(&format!("couldn't start the heater: {e}")))
}

fn fail(msg: &str) -> ! {
    eprintln!("warm: {msg}");
    std::process::exit(1);
}

/// Non-interactive view: one status line a second. Ctrl-C stops the heater.
fn watch_plain(info: &Info) {
    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        let _ = ctrlc::set_handler(move || running.store(false, Ordering::Relaxed));
    }
    while running.load(Ordering::Relaxed) {
        match client::status(info) {
            Ok(s) => {
                print!("\r\x1b[K{}", status_line(&s));
                let _ = io::stdout().flush();
            }
            Err(_) => {
                println!("\nwarm stopped");
                return;
            }
        }
        thread::sleep(Duration::from_secs(1));
    }
    client::stop(info);
    println!("\nwarm stopped");
}

fn status_line(s: &Status) -> String {
    let temp = s.temp.map_or("--".into(), |t| format!("{t:.1}"));
    let mut line = format!(
        "{temp}°C → {:.1}°C  power {:.0}%  cpu {:.0}%",
        s.target,
        s.duty * 100.0,
        s.cpu_pct
    );
    if let Some(c) = s.cpu_c {
        line += &format!(" {c:.0}°C");
    }
    if let Some(p) = s.charge_pct {
        line += &format!("  batt {p:.0}%");
    }
    if let (true, Some(w)) = (s.on_battery, s.watts) {
        line += &format!(" {w:.1}W");
    }
    line + &format!("  [{}]", s.state)
}

pub fn resolve_source(wanted: Source, r: &Reading) -> Option<Source> {
    let source = match wanted {
        Source::Auto if r.battery_c.is_some() => Source::Battery,
        Source::Auto => Source::Cpu,
        s => s,
    };
    pick(source, r).map(|_| source)
}

pub fn default_target(source: Source, opts: &Opts) -> f32 {
    opts.target.unwrap_or(if source == Source::Battery {
        35.0
    } else {
        55.0
    })
}

pub fn max_target(source: Source, opts: &Opts) -> f32 {
    if source == Source::Battery {
        BATTERY_MAX_C
    } else {
        opts.cpu_limit - 5.0
    }
}

pub fn pick(source: Source, r: &Reading) -> Option<f32> {
    match source {
        Source::Battery | Source::Auto => r.battery_c,
        Source::Cpu => r.cpu_c,
    }
}

pub fn name(source: Source) -> &'static str {
    match source {
        Source::Auto => "auto",
        Source::Battery => "battery",
        Source::Cpu => "cpu",
    }
}

fn list() {
    let mut sensors = Sensors::new();
    let r = sensors.read();
    let all = sensors.list();
    if all.is_empty() {
        println!("no thermal sensors visible");
    }
    for (label, t) in all {
        println!("{t:6.1}°C  {label}");
    }
    println!();
    let fmt = |t: Option<f32>| t.map_or("none".into(), |t| format!("{t:.1}°C"));
    println!("cpu (hottest):  {}", fmt(r.cpu_c));
    println!("battery:        {}", fmt(r.battery_c));
    if let Some(p) = r.charge_pct {
        let power = if r.on_battery {
            "on battery"
        } else {
            "plugged in"
        };
        println!("charge:         {p:.0}% ({power})");
    }
}
