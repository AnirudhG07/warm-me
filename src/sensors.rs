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
/// ignored; kernel time is skipped because OSes throttle a hot chip by
/// injecting it, which would make the heater switch itself off.
pub struct Load {
    sys: System,
    pid: Pid,
}

/// A process at or above this share of one core counts as "you working".
const BUSY_PROCESS: f32 = 0.3;

impl Load {
    /// None if this platform can't report CPU usage; we then never yield.
    pub fn new() -> Option<Self> {
        let sys = System::new().ok()?;
        let pid = sysinfo::get_current_pid().ok()?;
        let mut load = Load { sys, pid };
        load.busy();
        Some(load)
    }

    /// Usage since the previous call.
    pub fn busy(&mut self) -> f32 {
        self.sys.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cpu(),
        );
        self.sys
            .processes()
            .values()
            .filter(|p| p.pid() != self.pid && !is_kernel(p))
            .map(|p| p.cpu_usage() / 100.0)
            .filter(|&c| c >= BUSY_PROCESS)
            .fold(0.0, |a, c| a + c)
    }
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

fn is_cpu(label: &str) -> bool {
    let l = label.to_lowercase();
    CPU_HINTS.iter().any(|h| l.contains(h))
}
