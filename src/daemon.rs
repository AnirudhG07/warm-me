//! The heater: one per user, detached from any terminal so it keeps going
//! after the UI closes. Holds the single-instance lock, runs the control
//! loop, and answers `warm` / `warm on` / `warm off` over localhost.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::client::{self, Info, Status};
use crate::gpu::Gpu;
use crate::heater::Heater;
use crate::work::Work;
use crate::sensors::{Load, Reading, Sensors, Usage};
use crate::{Opts, Source, name, pick};

const HISTORY: usize = 600; // ten minutes at one sample a second
/// Smoothing for busy-program demand. Apps like browsers and chat clients
/// spike past half a core for a split second many times a minute; yielding
/// to each spike kept the heater near half power. Real work (a build, a
/// game) stays busy for seconds, so it still shows through.
const BUSY_TAU: f32 = 2.0; // s

/// Warming up from cold: further than this below target, the heater runs at
/// normal priority and only gives way to heavy work, so the laptop warms
/// noticeably sooner. Within BOOST_OFF it drops back to the lowest priority
/// to hold the temperature quietly. The gap between the two stops it
/// flapping on sensor noise.
const BOOST_ON: f32 = 1.5; // °C below target
const BOOST_OFF: f32 = 0.5; // °C below target

/// Settings the UI can change while we run, plus the latest status.
struct Shared {
    target: f32,
    max_duty: f32,
    max_target: f32,
    status: Status,
}

pub fn run(opts: Opts) {
    // a second heater (e.g. two `warm on`s racing) just bows out
    let Some(_lock) = client::acquire_lock() else {
        return;
    };
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }

    let mut sensors = Sensors::new();
    let first = sensors.read();
    let Some(source) = crate::resolve_source(opts.sensor, &first) else {
        eprintln!("no {} temperature sensor found", name(opts.sensor));
        return;
    };
    let max_target = crate::max_target(source, &opts);
    // start the project before anything else, so a missing one is reported
    // straight back to the terminal that asked for it
    let work = match Work::start(&opts.work_spec()) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}");
            return;
        }
    };
    let threads = opts
        .threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(4, |n| n.get()));

    let listener = match TcpListener::bind(("127.0.0.1", 0)) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("can't listen on localhost: {e}");
            return;
        }
    };
    let info = Info {
        pid: std::process::id(),
        port: listener.local_addr().map_or(0, |a| a.port()),
        token: client::new_token(),
    };
    if let Err(e) = client::publish(&info) {
        eprintln!("can't write state file: {e}");
        return;
    }

    let shared = Arc::new(Mutex::new(Shared {
        target: crate::default_target(source, &opts).min(max_target),
        max_duty: opts.max_duty.map_or(1.0, |m| (m / 100.0).clamp(0.0, 1.0)),
        max_target,
        status: Status::default(),
    }));
    let running = Arc::new(AtomicBool::new(true));
    {
        // a polite kill still shuts down cleanly: a paused project must
        // never be left paused
        let running = running.clone();
        let _ = ctrlc::set_handler(move || running.store(false, Ordering::Relaxed));
    }
    {
        let (shared, running, token) = (shared.clone(), running.clone(), info.token.clone());
        thread::spawn(move || serve(listener, &token, &shared, &running));
    }

    engine(&opts, source, threads, sensors, work, &shared, &running);
    client::unpublish();
}

/// Answer one-line requests from clients until the process exits.
fn serve(listener: TcpListener, token: &str, shared: &Mutex<Shared>, running: &AtomicBool) {
    for stream in listener.incoming().flatten() {
        let _ = handle(stream, token, shared, running);
    }
}

fn handle(
    stream: TcpStream,
    token: &str,
    shared: &Mutex<Shared>,
    running: &AtomicBool,
) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let mut words = line.split_whitespace();
    if words.next() != Some(token) {
        return Ok(()); // not one of ours: say nothing
    }
    let arg = |w: Option<&str>| w.and_then(|v| v.parse::<f32>().ok());
    let reply = {
        let mut s = shared.lock().unwrap();
        match words.next() {
            // empty until the first reading; clients wait for a real one
            Some("status") if s.status.sensor.is_empty() => "error: starting".into(),
            Some("status") => {
                // settings changed since the last sample show up immediately
                let mut status = s.status.clone();
                (status.target, status.max_duty) = (s.target, s.max_duty);
                serde_json::to_string(&status)?
            }
            Some("target") => match arg(words.next()) {
                Some(t) => {
                    s.target = t.clamp(5.0, s.max_target);
                    "ok".into()
                }
                None => "error: target needs a number".into(),
            },
            Some("cap") => match arg(words.next()) {
                Some(c) => {
                    s.max_duty = c.clamp(0.0, 1.0);
                    "ok".into()
                }
                None => "error: cap needs a number".into(),
            },
            Some("stop") => {
                running.store(false, Ordering::Relaxed);
                "ok".into()
            }
            _ => "error: unknown command".into(),
        }
    };
    writeln!(&stream, "{reply}")
}

fn engine(
    opts: &Opts,
    source: Source,
    threads: usize,
    mut sensors: Sensors,
    mut work: Option<Work>,
    shared: &Mutex<Shared>,
    running: &AtomicBool,
) {
    let heater = Heater::start(threads);
    // the GPU joins in only while boosting: it's the biggest heat source
    // there is, but also the one your screen needs
    let gpu = if opts.no_gpu { None } else { Gpu::start() };
    if let Some(g) = &gpu {
        eprintln!("gpu heater ready: {}", g.name);
    }
    let mut load = Load::new();
    if let Some(l) = &mut load {
        l.watch(work.as_ref().map(|w| w.root));
    }
    let mut ended: Option<String> = None;
    let mut project = 0.0f32; // project's share of all cores, smoothed
    let mut pi = Pi::new();
    let mut history: VecDeque<Option<f32>> = VecDeque::with_capacity(HISTORY);
    let (mut want, mut state) = (0.0, String::new());
    let (mut usage, mut busy, mut room) = (Usage::default(), 0.0f32, 0.0f32);
    let (mut boost, mut urgency) = (false, 0.0f32);
    let mut r = Reading::default();

    let started = Instant::now();
    let tick = Duration::from_millis(250);
    let mut last_sample: Option<Instant> = None;

    while running.load(Ordering::Relaxed) {
        let (target, max_duty) = {
            let s = shared.lock().unwrap();
            (s.target, s.max_duty)
        };

        // every second: read sensors, regulate, publish status
        let sample = last_sample.is_none_or(|t| t.elapsed() >= Duration::from_secs(1));
        if sample {
            let dt = last_sample.map_or(1.0, |t| t.elapsed().as_secs_f32());
            last_sample = Some(Instant::now());
            r = sensors.read();
            (want, state) = control(opts, source, target, &r, &mut pi, dt);
            let gap = pick(source, &r).map_or(0.0, |t| target - t);
            // how hard to push: 0 near the target, 1 when far below it
            urgency = if opts.no_boost || want == 0.0 {
                0.0
            } else {
                ((gap - BOOST_OFF) / 2.5).clamp(0.0, 1.0)
            };
            if urgency == 0.0 || gap < BOOST_OFF {
                boost = false;
            } else if gap > BOOST_ON {
                boost = true;
            }
            heater.set_boost(boost);
            // the project quit or went away: warm's own heater takes over
            if let Some(w) = work.take_if(|w| !w.alive()) {
                ended = Some(format!("{} stopped", w.label));
                w.release();
                project = 0.0;
                if let Some(l) = &mut load {
                    l.watch(None);
                }
            }
            if history.len() == HISTORY {
                history.pop_front();
            }
            history.push_back(pick(source, &r));
        }

        // every 250 ms: your work changes fast. Busy siblings drag down shared
        // clocks and power budget, so the heater gives way — your work is
        // producing heat anyway.
        usage = load.as_mut().map_or(usage, Load::sample);
        busy += (usage.busy - busy) * (tick.as_secs_f32() / BUSY_TAU);
        // far from target, give way only to heavier work (up to two busy
        // cores before stopping); near it, step aside for a single one
        let allowed = if opts.no_yield {
            1.0
        } else {
            yield_room(busy * (1.0 - 0.5 * urgency))
        };
        // yield instantly, come back gently so brief spikes don't cause stutter
        room = allowed.min(room + 0.1);
        let duty = want.min(max_duty) * room;
        let ncpu = threads.max(1) as f32;
        project += (usage.project / ncpu - project) * (tick.as_secs_f32() / BUSY_TAU);
        // the project does what it can; warm's own heater tops up the rest
        let fill = match &mut work {
            Some(w) => {
                w.set(duty, load.as_ref().map_or(&[], |l| l.tree()));
                (duty - project).clamp(0.0, 1.0)
            }
            None => duty,
        };
        heater.set_duty(fill);
        let gpu_duty = if boost { duty } else { 0.0 };
        if let Some(g) = &gpu {
            g.set_duty(gpu_duty);
        }

        if sample {
            let shown = if want.min(max_duty) > 0.0 && allowed < 1.0 {
                "yielding to your work"
            } else if boost {
                "warming up · boost"
            } else if want > max_duty {
                "heating (capped)"
            } else {
                &state
            };
            let status = Status {
                sensor: name(source).into(),
                uptime_s: started.elapsed().as_secs(),
                temp: pick(source, &r),
                target,
                state: shown.into(),
                duty,
                max_duty,
                cpu_pct: usage.total_pct,
                ours_pct: usage.ours_pct,
                cpu_c: r.cpu_c,
                battery_c: r.battery_c,
                charge_pct: r.charge_pct,
                on_battery: r.on_battery,
                watts: r.watts,
                gpu: gpu.as_ref().map(|_| gpu_duty),
                work: match (&work, &ended) {
                    (Some(w), _) => w.label.clone(),
                    (None, Some(e)) => format!("built-in heat ({e})"),
                    (None, None) => "built-in heat".into(),
                },
                work_pct: work.as_ref().map(|_| project * 100.0),
                history: history.iter().copied().collect(),
            };
            shared.lock().unwrap().status = status;
        }
        thread::sleep(tick);
    }
    heater.stop();
    if let Some(w) = work {
        w.release();
    }
}

/// Share of the heater allowed given sustained busy-program demand (in
/// cores): full heat up to half a core of background chatter, fully off once
/// something keeps a whole core busy.
fn yield_room(busy: f32) -> f32 {
    ((1.0 - busy) * 2.0).clamp(0.0, 1.0)
}

/// Desired duty from temperature and battery state, before yielding.
fn control(
    opts: &Opts,
    source: Source,
    target: f32,
    r: &Reading,
    pi: &mut Pi,
    dt: f32,
) -> (f32, String) {
    let (duty, state) = match pick(source, r) {
        None => (0.0, "sensor lost".to_string()),
        Some(_) if opts.plugged_only && r.on_battery => (0.0, "on battery, paused".into()),
        Some(_) if r.on_battery && r.charge_pct.is_some_and(|p| p < opts.min_battery) => {
            (0.0, "low battery, paused".into())
        }
        Some(t) => {
            let mut d = pi.update(target, t, dt);
            let mut state = match d {
                0.0 => "at target",
                _ if target - t <= BOOST_OFF => "holding",
                _ => "heating",
            }
            .to_string();
            if r.cpu_c.is_some_and(|c| c > opts.cpu_limit) {
                d *= 0.5;
                state = "cpu hot, backing off".into();
            }
            (d, state)
        }
    };
    if duty == 0.0 && state.contains("paused") {
        pi.reset();
    }
    if r.charging && r.battery_c.is_some_and(|t| t < 0.0) {
        return (duty, "charging below 0°C — unplug until warm".into());
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
    const KP: f32 = 0.45; // full power when ~2°C below target
    const KI: f32 = 0.002; // per °C per second
    const ON_COURSE: f32 = 600.0; // s: arriving within this is good enough
    const SLOPE_TAU: f32 = 60.0; // s: smoothing for the rate estimate
    const LOOKAHEAD: f32 = 400.0; // s: act on where the temperature is heading

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

#[cfg(test)]
mod tests {
    use super::Pi;

    /// Two-node thermal model: the heater warms the chassis, which slowly
    /// warms the battery. Time constants roughly match an M1 laptop, where
    /// minutes at full power moved the battery only a fraction of a degree.
    fn simulate(ambient: f32, target: f32, minutes: u32) -> (f32, f32, u32) {
        const GAIN: f32 = 12.0; // chassis rise above ambient at full duty, °C
        const TAU_CHASSIS: f32 = 300.0;
        const TAU_BATTERY: f32 = 900.0;
        let dt = 2.0;
        let (mut chassis, mut battery) = (ambient, ambient);
        let mut pi = Pi::new();
        let (mut peak, mut reached) = (battery, 0);
        for step in 0..(minutes * 30) {
            let duty = pi.update(target, battery, dt);
            chassis += (GAIN * duty - (chassis - ambient)) / TAU_CHASSIS * dt;
            battery += (chassis - battery) / TAU_BATTERY * dt;
            peak = peak.max(battery);
            if reached == 0 && battery >= target - 0.3 {
                reached = step * 2 / 60;
            }
        }
        (peak, battery, reached)
    }

    #[test]
    fn reaches_and_holds_target_without_big_overshoot() {
        for (ambient, target) in [(5.0, 15.0), (20.0, 30.0), (28.0, 35.0)] {
            let (peak, end, reached) = simulate(ambient, target, 240);
            println!(
                "ambient {ambient} target {target}: within 0.3°C after {reached} min, peak {peak:.2}, end {end:.2}"
            );
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
        let (_, end, _) = simulate(0.0, 15.0, 240);
        assert!(end > 10.0 && end < 15.0);
    }
}
