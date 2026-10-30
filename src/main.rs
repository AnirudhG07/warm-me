//! warm-me — keep a laptop warm in the cold by turning spare CPU into heat.
//!
//! A PI controller drives a duty-cycled, lowest-priority workload until the
//! chosen sensor (battery by default) reaches the target, then holds it there.

mod heater;
mod sensors;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};

use heater::Heater;
use sensors::{Load, Reading, Sensors};

/// Battery chemistry limit: don't push the pack itself past this.
const BATTERY_MAX_C: f32 = 40.0;

#[derive(Parser)]
#[command(
    version,
    about = "Keep your laptop warm in the cold by turning spare CPU into heat"
)]
struct Args {
    /// Temperature to hold, in °C [default: 35 for battery, 55 for cpu]
    target: Option<f32>,

    /// Which sensor to regulate on
    #[arg(short, long, value_enum, default_value_t = Source::Auto)]
    sensor: Source,

    /// Worker threads [default: all logical cores]
    #[arg(short, long)]
    threads: Option<usize>,

    /// Upper bound on heater duty, in percent (lower = gentler, quieter)
    #[arg(short = 'm', long, default_value_t = 100.0)]
    max_duty: f32,

    /// Pause when on battery below this charge, in percent
    #[arg(long, default_value_t = 20.0)]
    min_battery: f32,

    /// Only heat while plugged in (heating on battery costs charge)
    #[arg(short, long)]
    plugged_only: bool,

    /// Back off if the CPU gets hotter than this, in °C
    #[arg(long, default_value_t = 95.0)]
    cpu_limit: f32,

    /// Keep heating at full duty even while you're using the CPU
    #[arg(long)]
    no_yield: bool,

    /// List detected sensors and exit
    #[arg(short, long)]
    list: bool,
}

#[derive(Clone, Copy, PartialEq, ValueEnum)]
enum Source {
    /// Battery if it reports a temperature, else CPU
    Auto,
    /// Battery pack temperature: best proxy for how warm the laptop feels
    Battery,
    /// Hottest CPU/SoC die sensor: reacts fast, but runs far above chassis temp
    Cpu,
}

fn main() {
    let args = Args::parse();
    let mut sensors = Sensors::new();
    let first = sensors.read();

    if args.list {
        list(&mut sensors, &first);
        return;
    }

    let source = match args.sensor {
        Source::Auto if first.battery_c.is_some() => Source::Battery,
        Source::Auto => Source::Cpu,
        s => s,
    };
    if pick(source, &first).is_none() {
        eprintln!(
            "no {} temperature sensor found; try `warm-me --list`",
            name(source)
        );
        std::process::exit(1);
    }

    let mut target = args.target.unwrap_or(if source == Source::Battery {
        35.0
    } else {
        55.0
    });
    if source == Source::Battery && target > BATTERY_MAX_C {
        eprintln!("target capped at {BATTERY_MAX_C}°C to protect the battery");
        target = BATTERY_MAX_C;
    }
    let threads = args
        .threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(4, |n| n.get()));
    let max_duty = (args.max_duty / 100.0).clamp(0.0, 1.0);

    let running = Arc::new(AtomicBool::new(true));
    {
        let running = running.clone();
        ctrlc::set_handler(move || running.store(false, Ordering::Relaxed))
            .expect("ctrl-c handler");
    }

    println!(
        "warming {} sensor to {target:.1}°C with {threads} workers (ctrl-c to stop)",
        name(source)
    );

    let heater = Heater::start(threads);
    let mut load = Load::new();
    let mut pi = Pi::new();
    let control_every = Duration::from_secs(2);
    let mut last_control: Option<Instant> = None;
    let (mut r, mut want, mut state) = (first, 0.0, String::new());
    let mut room: f32 = 0.0;

    while running.load(Ordering::Relaxed) {
        // temperature changes slowly: regulate every couple of seconds
        if last_control.is_none_or(|t| t.elapsed() >= control_every) {
            let dt = last_control.map_or(2.0, |t| t.elapsed().as_secs_f32());
            last_control = Some(Instant::now());
            r = sensors.read();
            (want, state) = control(&args, source, target, max_duty, &r, &mut pi, dt);
        }

        // your work changes fast: re-check every 250 ms. Busy siblings drag down
        // shared clocks and power budget, so the heater gives way — your work
        // is producing heat anyway.
        let busy = load.as_mut().map_or(0.0, Load::busy);
        let allowed = if args.no_yield { 1.0 } else { yield_room(busy) };
        // yield instantly, come back gently so brief spikes don't cause stutter
        room = allowed.min(room + 0.1);
        let duty = want * room;
        heater.set_duty(duty);

        let shown = if want > 0.0 && room < 1.0 {
            "yielding to your work"
        } else {
            &state
        };
        status(&r, source, target, duty, busy, shown);
        thread::sleep(Duration::from_millis(250));
    }

    heater.stop();
    println!("\nstopped.");
}

/// Share of the heater allowed given busy-program demand (in cores):
/// fully off once a program wants most of a core.
fn yield_room(busy: f32) -> f32 {
    (1.0 - busy / 0.6).clamp(0.0, 1.0)
}

/// Desired duty from temperature and battery state, before yielding.
fn control(
    args: &Args,
    source: Source,
    target: f32,
    max_duty: f32,
    r: &Reading,
    pi: &mut Pi,
    dt: f32,
) -> (f32, String) {
    let (duty, state) = match pick(source, r) {
        None => (0.0, "sensor lost".to_string()),
        Some(_) if args.plugged_only && r.on_battery => (0.0, "on battery, paused".into()),
        Some(_) if r.on_battery && r.charge_pct.is_some_and(|p| p < args.min_battery) => {
            (0.0, "low battery, paused".into())
        }
        Some(t) => {
            let mut d = pi.update(target, t, dt).min(max_duty);
            let mut state = if d > 0.0 { "heating" } else { "at target" }.to_string();
            if r.cpu_c.is_some_and(|c| c > args.cpu_limit) {
                d *= 0.5;
                state = "cpu hot, backing off".into();
            }
            (d, state)
        }
    };
    if duty == 0.0 && state.contains("paused") {
        pi.reset();
    }
    (duty, state)
}

/// Proportional-integral controller producing duty in 0..=1.
/// P reacts to the gap; I slowly learns the steady duty that holds the
/// target on this particular machine, in this particular room.
///
/// Battery temperature lags heat input by minutes. A plain integral winds up
/// to full power during the long climb and then overshoots, so it only
/// learns when progress stalls: if we're already on course to reach the
/// target soon, more heat isn't needed.
struct Pi {
    integral: f32,
    /// Smoothed rate of temperature change, °C/s.
    slope: f32,
    last: Option<f32>,
}

impl Pi {
    const KP: f32 = 0.3; // full power when ~3°C below target
    const KI: f32 = 0.002; // per °C per second
    const ON_COURSE: f32 = 600.0; // s: arriving within this is good enough
    const SLOPE_TAU: f32 = 60.0; // s: smoothing for the rate estimate
    const LOOKAHEAD: f32 = 600.0; // s: act on where the temperature is heading

    fn new() -> Self {
        Pi {
            integral: 0.0,
            slope: 0.0,
            last: None,
        }
    }

    fn reset(&mut self) {
        *self = Pi::new();
    }

    fn update(&mut self, target: f32, temp: f32, dt: f32) -> f32 {
        if let Some(last) = self.last {
            let rate = (temp - last) / dt;
            self.slope += (rate - self.slope) * (dt / Self::SLOPE_TAU).min(1.0);
        }
        self.last = Some(temp);

        let error = target - temp;
        // heading toward the target fast enough: let the lag catch up
        let on_course = error * self.slope > 0.0 && error / self.slope < Self::ON_COURSE;
        if !on_course {
            self.integral = (self.integral + Self::KI * error * dt).clamp(0.0, 1.0);
        }
        // P acts on the predicted temperature: eases off before the lag
        // carries us past the target
        let predicted = error - self.slope * Self::LOOKAHEAD;
        (Self::KP * predicted + self.integral).clamp(0.0, 1.0)
    }
}

fn pick(source: Source, r: &Reading) -> Option<f32> {
    match source {
        Source::Battery | Source::Auto => r.battery_c,
        Source::Cpu => r.cpu_c,
    }
}

fn name(source: Source) -> &'static str {
    match source {
        Source::Battery | Source::Auto => "battery",
        Source::Cpu => "cpu",
    }
}

fn status(r: &Reading, source: Source, target: f32, duty: f32, others: f32, state: &str) {
    let temp = pick(source, r).map_or("--".into(), |t| format!("{t:.1}"));
    let mut line = format!(
        "\r\x1b[K{temp}°C → {target:.1}°C  duty {:>3.0}%",
        duty * 100.0
    );
    line += &format!("  load {others:.1}");
    if let (Source::Battery, Some(c)) = (source, r.cpu_c) {
        line += &format!("  cpu {c:.0}°C");
    }
    if let Some(p) = r.charge_pct {
        line += &format!("  batt {p:.0}% {}", if r.on_battery { "DC" } else { "AC" });
    }
    if let (true, Some(w)) = (r.on_battery, r.watts) {
        line += &format!(" {w:.1}W");
    }
    if r.charging && r.battery_c.is_some_and(|t| t < 0.0) {
        line += "  ⚠ charging below 0°C";
    }
    line += &format!("  [{state}]");
    print!("{line}");
    let _ = std::io::stdout().flush();
}

fn list(sensors: &mut Sensors, r: &Reading) {
    let all = sensors.list();
    if all.is_empty() {
        println!("no thermal sensors visible");
    }
    for (label, t) in all {
        println!("{t:6.1}°C  {label}");
    }
    println!();
    println!(
        "cpu (hottest):  {}",
        r.cpu_c.map_or("none".into(), |t| format!("{t:.1}°C"))
    );
    println!(
        "battery:        {}",
        r.battery_c.map_or("none".into(), |t| format!("{t:.1}°C"))
    );
    if let Some(p) = r.charge_pct {
        println!(
            "charge:         {p:.0}% ({})",
            if r.on_battery {
                "on battery"
            } else {
                "plugged in"
            }
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Pi;

    /// Two-node thermal model: the heater warms the chassis, which slowly
    /// warms the battery. Time constants roughly match an M1 laptop, where
    /// minutes at full power moved the battery only a fraction of a degree.
    fn simulate(ambient: f32, target: f32, minutes: u32) -> (f32, f32) {
        const GAIN: f32 = 12.0; // chassis rise above ambient at full duty, °C
        const TAU_CHASSIS: f32 = 300.0;
        const TAU_BATTERY: f32 = 900.0;
        let dt = 2.0;
        let (mut chassis, mut battery) = (ambient, ambient);
        let mut pi = Pi::new();
        let mut peak = battery;
        for _ in 0..(minutes * 30) {
            let duty = pi.update(target, battery, dt);
            chassis += (GAIN * duty - (chassis - ambient)) / TAU_CHASSIS * dt;
            battery += (chassis - battery) / TAU_BATTERY * dt;
            peak = peak.max(battery);
        }
        (peak, battery)
    }

    #[test]
    fn reaches_and_holds_target_without_big_overshoot() {
        for (ambient, target) in [(5.0, 15.0), (20.0, 30.0), (28.0, 35.0)] {
            let (peak, end) = simulate(ambient, target, 240);
            println!("ambient {ambient} target {target}: peak {peak:.2}, end {end:.2}");
            assert!(
                peak - target < 0.5,
                "ambient {ambient}: overshoot to {peak:.2}"
            );
            assert!(
                (end - target).abs() < 0.3,
                "ambient {ambient}: settled at {end:.2}"
            );
        }
    }

    #[test]
    fn gives_up_gracefully_when_target_is_out_of_reach() {
        // can't get 15°C above ambient with this heater: should sit at full power
        let (_, end) = simulate(0.0, 15.0, 240);
        assert!(end > 10.0 && end < 15.0);
    }
}
