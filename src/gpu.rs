//! Second heat source for boost mode: the GPU, which on most laptops can
//! draw about as much power again as the CPU. Runs a compute shader of long
//! fused multiply-add chains in short batches (~2 ms each), duty-cycled like
//! the CPU workers. Short batches leave gaps for the desktop to draw frames,
//! so the screen stays smooth. Works through wgpu: Metal on macOS, Vulkan on
//! Linux, DirectX 12 on Windows.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const PERIOD: Duration = Duration::from_millis(20);
const BATCH: Duration = Duration::from_millis(2);
const DUTY_SCALE: f32 = 10_000.0;
const WORKGROUP: u32 = 256;
const BUF_ITEMS: u64 = 1 << 20; // vec4<f32> each: 16 MB

const SHADER: &str = r#"
@group(0) @binding(0) var<storage, read_write> buf: array<vec4<f32>>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let i = id.x % arrayLength(&buf);
    var a = buf[i];
    var b = a + vec4<f32>(0.1);
    var c = a + vec4<f32>(0.2);
    var d = a + vec4<f32>(0.3);
    let m = vec4<f32>(0.9999);
    let k = vec4<f32>(0.0001);
    // four independent chains keep the ALUs full; converges to 1.0
    for (var n = 0u; n < 1024u; n++) {
        a = fma(a, m, k);
        b = fma(b, m, k);
        c = fma(c, m, k);
        d = fma(d, m, k);
    }
    buf[i] = (a + b + c + d) * 0.25;
}
"#;

pub struct Gpu {
    pub name: String,
    duty: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

impl Gpu {
    /// None if there's no usable GPU; the CPU heater carries on alone.
    pub fn start() -> Option<Self> {
        let kernel = Kernel::new()?;
        let name = kernel.name.clone();
        let duty = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (duty, stop) = (duty.clone(), stop.clone());
            thread::Builder::new()
                .name("warm-me-gpu".into())
                .spawn(move || run(kernel, &duty, &stop))
                .ok()?
        };
        Some(Gpu {
            name,
            duty,
            stop,
            worker: Some(worker),
        })
    }

    pub fn set_duty(&self, duty: f32) {
        let d = (duty.clamp(0.0, 1.0) * DUTY_SCALE) as u32;
        self.duty.store(d, Ordering::Relaxed);
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

fn run(mut kernel: Kernel, duty: &AtomicU32, stop: &AtomicBool) {
    kernel.calibrate();
    while !stop.load(Ordering::Relaxed) {
        let d = duty.load(Ordering::Relaxed) as f32 / DUTY_SCALE;
        if d <= 0.0 {
            thread::sleep(Duration::from_millis(100));
            continue;
        }
        let start = Instant::now();
        let busy = PERIOD.mul_f32(d);
        while start.elapsed() < busy {
            kernel.batch();
        }
        if let Some(rest) = PERIOD.checked_sub(start.elapsed()) {
            thread::sleep(rest);
        }
    }
}

struct Kernel {
    name: String,
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    /// Workgroups per batch, tuned so one batch takes about BATCH.
    groups: u32,
}

impl Kernel {
    fn new() -> Option<Self> {
        let instance = wgpu::Instance::default();
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        }))
        .ok()?;
        let name = adapter.get_info().name;
        let (device, queue) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()?;

        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("warm heat"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("warm heat"),
            layout: None,
            module: &module,
            entry_point: Some("main"),
            compilation_options: Default::default(),
            cache: None,
        });
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("warm heat"),
            size: BUF_ITEMS * 16,
            usage: wgpu::BufferUsages::STORAGE,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("warm heat"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: buffer.as_entire_binding(),
            }],
        });
        Some(Kernel {
            name,
            device,
            queue,
            pipeline,
            bind_group,
            groups: 64,
        })
    }

    /// Submit one batch and wait for it, so pacing follows real GPU time.
    fn batch(&mut self) -> Duration {
        let start = Instant::now();
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_compute_pass(&Default::default());
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            pass.dispatch_workgroups(self.groups, 1, 1);
        }
        self.queue.submit([encoder.finish()]);
        let _ = self.device.poll(wgpu::PollType::wait_indefinitely());
        start.elapsed()
    }

    /// Size batches to about BATCH on this GPU: fast GPUs get more work per
    /// batch, slow ones less, so pacing and screen smoothness are the same.
    fn calibrate(&mut self) {
        let max = (BUF_ITEMS / WORKGROUP as u64) as u32 * 4;
        for _ in 0..8 {
            let took = self.batch().max(Duration::from_micros(50));
            let scale = BATCH.as_secs_f32() / took.as_secs_f32();
            self.groups = ((self.groups as f32 * scale) as u32).clamp(1, max);
        }
    }
}
