//! Issue #10 spike: wgpu renderer-API path (input to ADR-0004).
//!
//! Headless by design: with no GPU adapter (CI runners, this sandbox) the
//! spike reports the structured init result and the planned workload, and
//! exits 0. With an adapter present it creates a device with validation
//! enabled, validates a WGSL compute shader, builds the 10k-instanced-marker
//! buffers, and dispatches one compute pass. The wgpu API used here is
//! entirely safe Rust (zero `unsafe` blocks in this file).

use std::time::Instant;

const COMPUTE_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read_write> data: array<u32>;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let index = id.x;
    if (index < arrayLength(&data)) {
        data[index] = data[index] ^ 0x5EED_0001u;
    }
}
"#;

/// The shared narrow workload both candidates are measured against:
/// a grid terrain-like mesh plus 10,000 instanced unit markers (one instanced
/// draw call, one `vec4<f32>` per instance) and a 4 KiB compute buffer update.
const INSTANCED_MARKERS: usize = 10_000;
const INSTANCE_BUFFER_BYTES: usize = INSTANCED_MARKERS * 16;
const COMPUTE_BUFFER_BYTES: usize = 4096;

fn print_planned_workload() {
    println!(
        "WORKLOAD candidate=wgpu instanced_markers={INSTANCED_MARKERS} instance_buffer_bytes={INSTANCE_BUFFER_BYTES} \
         terrain_mesh=grid draw_calls_planned=1 compute_buffer_bytes={COMPUTE_BUFFER_BYTES} \
         note=\"rendering not executed headless (no surface); compute is executed and result-verified when a GPU adapter is present\""
    );
}

fn main() {
    println!("INIT candidate=wgpu version=30.0.1 backends=all validation=on unsafe_blocks=0");

    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::all();
    descriptor.flags = wgpu::InstanceFlags::VALIDATION;
    let instance = wgpu::Instance::new(descriptor);

    let adapter = match pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    })) {
        Ok(adapter) => adapter,
        Err(error) => {
            println!("ADAPTER candidate=wgpu found=false error=\"{error}\"");
            print_planned_workload();
            return;
        }
    };

    let info = adapter.get_info();
    println!(
        "ADAPTER candidate=wgpu found=true name=\"{}\" vendor={:#x} device={:#x} type={:?}",
        info.name, info.vendor, info.device, info.device_type
    );

    let (device, queue) =
        match pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: None,
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::default(),
            ..Default::default()
        })) {
            Ok(pair) => pair,
            Err(error) => {
                println!("DEVICE candidate=wgpu created=false error=\"{error}\"");
                print_planned_workload();
                return;
            }
        };

    let limits = device.limits();
    println!(
        "DEVICE candidate=wgpu created=true max_texture_dimension_2d={} max_compute_workgroup_size_x={} max_compute_invocations_per_workgroup={}",
        limits.max_texture_dimension_2d,
        limits.max_compute_workgroup_size_x,
        limits.max_compute_invocations_per_workgroup
    );

    // Shader workflow: WGSL is text, validated at module creation.
    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("spike-compute"),
        source: wgpu::ShaderSource::Wgsl(COMPUTE_WGSL.into()),
    });
    println!("SHADER candidate=wgpu stage=compute language=wgsl validated=true");

    // Workload resources: instance buffer for 10k markers + 4 KiB compute buffer.
    let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("instance-markers"),
        size: INSTANCE_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let compute_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("compute"),
        size: COMPUTE_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    queue.write_buffer(&compute_buffer, 0, &vec![0u8; COMPUTE_BUFFER_BYTES]);
    println!(
        "RESOURCES candidate=wgpu instance_buffer_bytes={} compute_buffer_bytes={} sync=\"managed by wgpu (resource transitions tracked)\"",
        INSTANCE_BUFFER_BYTES, COMPUTE_BUFFER_BYTES
    );
    let _ = instance_buffer;

    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("spike-compute-bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("spike-compute-bg"),
        layout: &bind_group_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: compute_buffer.as_entire_binding(),
        }],
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("spike-compute-pipeline"),
        layout: None,
        module: &shader,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    // Read-back buffer so the compute result is verified, not just submitted.
    let read_buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("compute-readback"),
        size: COMPUTE_BUFFER_BYTES as u64,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    // Timing note: this measures CPU encode + submit + GPU wait, i.e.
    // end-to-end submission latency, not pure GPU execution time (which would
    // need GPU timestamp queries on real hardware).
    let start = Instant::now();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("spike-compute-encoder"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("spike-compute-pass"),
            ..Default::default()
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(64, 1, 1);
    }
    encoder.copy_buffer_to_buffer(
        &compute_buffer,
        0,
        &read_buffer,
        0,
        COMPUTE_BUFFER_BYTES as u64,
    );
    queue.submit([encoder.finish()]);
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let elapsed = start.elapsed();

    let (tx, rx) = std::sync::mpsc::channel();
    read_buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result.is_ok());
        });
    let _ = device.poll(wgpu::PollType::Wait {
        submission_index: None,
        timeout: None,
    });
    let mapped = rx.recv().unwrap_or(false);
    let verified = if mapped {
        let data = read_buffer
            .slice(..)
            .get_mapped_range()
            .expect("buffer mapped for reading");
        // Buffer was zero-initialized; every u32 must now be 0 ^ 0x5EED0001.
        let expected = 0x5EED_0001u32;
        let ok = data
            .as_chunks::<4>()
            .0
            .iter()
            .all(|chunk| u32::from_le_bytes(*chunk) == expected);
        drop(data);
        read_buffer.unmap();
        ok
    } else {
        false
    };
    println!(
        "COMPUTE candidate=wgpu dispatched=true workgroups=64 buffer_bytes={COMPUTE_BUFFER_BYTES} verified={verified} \
         time_ms={:.3} timing=\"cpu encode+submit+wait (end-to-end latency, not pure GPU time)\"",
        elapsed.as_secs_f64() * 1000.0
    );

    print_planned_workload();
}
