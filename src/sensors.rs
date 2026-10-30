//! Temperature and power readings, via cross-platform crates:
//! `sysinfo` for thermal sensors (hwmon on Linux, HID/SMC on macOS, WMI on
//! Windows) and `starship-battery` for battery temperature, charge and draw.

use starship_battery::units::{
    power::watt, ratio::percent, thermodynamic_temperature::degree_celsius,
};
use starship_battery::{Battery, Manager, State};
use sysinfo::{Components, Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System};

pub struct Sensors {
    components: Option<Components>,
    manager: Option<Manager>,
    battery: Option<Battery>,
}

#[derive(Default)]
pub struct Reading {
    pub battery_c: Option<f32>,
    /// Hottest CPU-like sensor.
    pub cpu_c: Option<f32>,
    pub on_battery: bool,
    pub charging: bool,
    pub charge_pct: Option<f32>,
    /// Battery discharge rate; only meaningful when on battery.
    pub watts: Option<f32>,
}

/// Label fragments that identify CPU/SoC die sensors across platforms.
/// Linux: coretemp/k10temp/zenpower ("Package id 0", "Core 0", "Tctl", "Tdie").
/// Apple Silicon: pACC/eACC are the P/E CPU clusters, "tdie"/"SOC Die" the SoC.
const CPU_HINTS: &[&str] = &[
    "cpu", "core", "package", "tdie", "tctl", "k10temp", "coretemp", "zenpower", "soc", "pacc",
    "eacc",
];

impl Sensors {
    pub fn new() -> Self {
        let components = Components::new_with_refreshed_list().ok();
        let manager = Manager::new().ok();
        let battery = manager
            .as_ref()
            .and_then(|m| m.batteries().ok())
            .and_then(|mut it| it.find_map(Result::ok));
        Sensors {
            components,
            manager,
            battery,
        }
    }

    pub fn read(&mut self) -> Reading {
        let mut r = Reading::default();

        if let Some(comps) = &mut self.components {
            comps.refresh(false);
            r.cpu_c = comps
                .iter()
                .filter(|c| is_cpu(c.label()))
                .filter_map(|c| c.temperature())
                .filter(|t| t.is_finite() && *t > 0.0)
                .reduce(f32::max);
        }

        if let (Some(m), Some(b)) = (&self.manager, &mut self.battery) {
            let _ = m.refresh(b);
            r.battery_c = b.temperature().map(|t| t.get::<degree_celsius>());
            r.charge_pct = Some(b.state_of_charge().get::<percent>());
            r.on_battery = matches!(b.state(), State::Discharging | State::Empty);
            r.charging = b.state() == State::Charging;
            r.watts = Some(b.energy_rate().get::<watt>()).filter(|w| *w > 0.0);
        }
        r
    }

    /// Every sensor we can see, for `--list`.
    pub fn list(&mut self) -> Vec<(String, f32)> {
        let mut out = Vec::new();
        if let Some(comps) = &mut self.components {
            comps.refresh(false);
            for c in comps.iter() {
                if let Some(t) = c.temperature() {
                    out.push((c.label().to_string(), t));
                }
            }
        }
        out
    }
}

/// Demand from busy programs, in cores: lets the heater step aside while
/// you're actually using the machine.
///
/// Counts only processes using a real share of a core. Background chatter
/// (browser tabs, window server) is spread thin across many processes and is
/// ignored; so is housekeeping the OS itself runs at background priority
/// (photo analysis, contact sync, indexing), which can keep a core busy for
/// minutes. Kernel time is skipped because OSes throttle a hot chip by
/// injecting it, which would make the heater switch itself off.
pub struct Load {
    sys: System,
    pid: Pid,
    /// A volunteer-computing project heating for us: its processes are
    /// heat, not "you working", so they're counted separately.
    root: Option<u32>,
    tree: Vec<u32>,
}

/// A process at or above this share of one core counts as "you working".
const BUSY_PROCESS: f32 = 0.5;

/// One sample of CPU activity since the previous call.
#[derive(Clone, Copy, Default)]
pub struct Usage {
    /// The watched project's use, in cores.
    pub project: f32,
    /// Demand from busy programs other than us, in cores.
    pub busy: f32,
    /// Whole-machine CPU use, percent of all cores.
    pub total_pct: f32,
    /// Our own share, percent of all cores.
    pub ours_pct: f32,
}

impl Load {
    /// None if this platform can't report CPU usage; we then never yield.
    pub fn new() -> Option<Self> {
        let sys = System::new().ok()?;
        let pid = sysinfo::get_current_pid().ok()?;
        let mut load = Load {
            sys,
            pid,
            root: None,
            tree: Vec::new(),
        };
        load.sample();
        Some(load)
    }

    /// Count this process and everything it starts as the project.
    pub fn watch(&mut self, root: Option<u32>) {
        self.root = root;
        self.tree.clear();
    }

    /// The watched project's processes as of the last sample.
    pub fn tree(&self) -> &[u32] {
        &self.tree
    }

    pub fn sample(&mut self) -> Usage {
        self.sys.refresh_cpu_usage();
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );
        let ncpu = self.sys.cpus().len().max(1) as f32;
        self.tree = match self.root {
            Some(root) => self
                .sys
                .processes()
                .values()
                .filter(|p| descends_from(&self.sys, p, root))
                .map(|p| p.pid().as_u32())
                .collect(),
            None => Vec::new(),
        };
        let in_tree = |p: &Process| self.tree.contains(&p.pid().as_u32());
        let project = self
            .sys
            .processes()
            .values()
            .filter(|p| in_tree(p))
            .fold(0.0, |a, p| a + p.cpu_usage() / 100.0);
        let busy = self
            .sys
            .processes()
            .values()
            .filter(|p| p.pid() != self.pid && p.cpu_usage() >= BUSY_PROCESS * 100.0)
            .filter(|p| !in_tree(p))
            .filter(|p| !is_kernel(p) && !is_background(p.pid().as_u32()))
            .fold(0.0, |a, p| a + p.cpu_usage() / 100.0);
        let ours = self.sys.process(self.pid).map_or(0.0, |p| p.cpu_usage());
        Usage {
            project,
            busy,
            total_pct: self.sys.global_cpu_usage(),
            ours_pct: (ours / ncpu).min(100.0),
        }
    }
}

/// The process itself or one of its descendants.
fn descends_from(sys: &System, p: &Process, root: u32) -> bool {
    let mut cur = Some(p);
    for _ in 0..64 {
        match cur {
            Some(c) if c.pid().as_u32() == root => return true,
            Some(c) => cur = c.parent().and_then(|pp| sys.process(pp)),
            None => return false,
        }
    }
    false
}

fn is_kernel(p: &Process) -> bool {
    let name = p.name().to_string_lossy();
    // macOS kernel_task, Windows "System"/idle, Linux kernel threads (no exe)
    p.pid().as_u32() == 0
        || name == "kernel_task"
        || name == "System"
        || (cfg!(target_os = "linux")
            && p.exe().is_none()
            && p.parent().is_some_and(|pp| pp.as_u32() == 2))
}

/// Running at background priority: work the OS or the program itself has
/// marked as not urgent, so heating alongside it costs nobody anything.
fn is_background(pid: u32) -> bool {
    #[cfg(target_os = "macos")]
    unsafe {
        // Darwin priority bands: 4 background, 20 utility, 31 default,
        // 37+ user-initiated / interactive.
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = size_of::<libc::proc_taskinfo>() as libc::c_int;
        let got = libc::proc_pidinfo(
            pid as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        );
        got == size && info.pti_priority < 20
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    unsafe {
        // niced daemons (indexers, updaters); errors return -1, so not these
        libc::getpriority(libc::PRIO_PROCESS, pid as libc::id_t) > 0
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn is_cpu(label: &str) -> bool {
    let l = label.to_lowercase();
    CPU_HINTS.iter().any(|h| l.contains(h))
}
