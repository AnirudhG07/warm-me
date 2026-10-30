//! Heat source: one low-priority worker per core running a duty-cycled
//! FP + memory workload. Duty 0.0..=1.0 maps to the share of each 10 ms
//! slice spent busy, so heat output is roughly proportional to duty.

use std::hint::black_box;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PERIOD: Duration = Duration::from_millis(10);
const DUTY_SCALE: f32 = 10_000.0;

pub struct Heater {
    duty: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    workers: Vec<JoinHandle<()>>,
}

impl Heater {
    pub fn start(threads: usize) -> Self {
        let duty = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let workers = (0..threads)
            .map(|id| {
                let (duty, stop) = (duty.clone(), stop.clone());
                // stagger phases so workers don't all spike at once
                let phase = PERIOD.mul_f32(id as f32 / threads as f32);
                thread::Builder::new()
                    .name(format!("warm-me-{id}"))
                    .spawn(move || worker(phase, &duty, &stop))
                    .expect("spawn worker")
            })
            .collect();
        Heater {
            duty,
            stop,
            workers,
        }
    }

    pub fn set_duty(&self, duty: f32) {
        let d = (duty.clamp(0.0, 1.0) * DUTY_SCALE) as u32;
        self.duty.store(d, Ordering::Relaxed);
    }

    pub fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        for w in self.workers {
            let _ = w.join();
        }
    }
}

fn worker(phase: Duration, duty: &AtomicU32, stop: &AtomicBool) {
    lower_thread_priority();
    let mut work = Work::new();
    thread::sleep(phase);
    while !stop.load(Ordering::Relaxed) {
        let d = duty.load(Ordering::Relaxed) as f32 / DUTY_SCALE;
        if d <= 0.0 {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let start = Instant::now();
        let busy = PERIOD.mul_f32(d);
        while start.elapsed() < busy {
            work.step();
        }
        if let Some(rest) = PERIOD.checked_sub(start.elapsed()) {
            thread::sleep(rest);
        }
    }
}

/// Mix of wide FP math (SIMD units) and streaming copies (memory controller):
/// both burn more power per cycle than a plain integer spin loop.
struct Work {
    acc: [f32; 64],
    src: Vec<u8>,
    dst: Vec<u8>,
    offset: usize,
    steps: u32,
}

const BUF: usize = 32 << 20; // well past last-level cache
const CHUNK: usize = 1 << 20;

impl Work {
    fn new() -> Self {
        let mut acc = [0.0; 64];
        for (i, x) in acc.iter_mut().enumerate() {
            *x = 1.0 + i as f32 * 1e-3;
        }
        Work {
            acc,
            src: vec![1; BUF],
            dst: vec![0; BUF],
            offset: 0,
            steps: 0,
        }
    }

    /// Roughly tens of microseconds of work, short enough for tight duty timing.
    fn step(&mut self) {
        // 64 independent multiply-add chains; converges toward 1.0, never denormal
        let (m, c) = (black_box(0.9999f32), black_box(1e-4f32));
        for _ in 0..256 {
            for x in self.acc.iter_mut() {
                *x = *x * m + c;
            }
        }
        black_box(&self.acc);

        self.steps = self.steps.wrapping_add(1);
        if self.steps.is_multiple_of(4) {
            let r = self.offset..self.offset + CHUNK;
            self.dst[r.clone()].copy_from_slice(&self.src[r]);
            black_box(&self.dst);
            self.offset = (self.offset + CHUNK) % BUF;
        }
    }
}

/// Lowest priority the OS offers that still lets us heat:
/// the scheduler hands the CPU to anything else that wants it.
fn lower_thread_priority() {
    #[cfg(target_os = "linux")]
    unsafe {
        // SCHED_IDLE: below nice 19, runs only on otherwise-idle CPU time.
        // pid 0 = the calling thread on Linux.
        let param = libc::sched_param { sched_priority: 0 };
        if libc::sched_setscheduler(0, libc::SCHED_IDLE, &param) != 0 {
            libc::setpriority(libc::PRIO_PROCESS, 0, 19);
        }
    }

    #[cfg(target_os = "macos")]
    unsafe {
        // Utility QoS yields to interactive work but may still use P-cores.
        // (Background QoS would pin us to E-cores, which make little heat.)
        libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_UTILITY, 0);
    }

    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::Threading::*;
        SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_IDLE);
    }

    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, 19);
    }
}
