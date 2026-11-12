//! Issue #10 spike: ash (raw Vulkan bindings) renderer-API path (input to
//! ADR-0004).
//!
//! Headless by design: without the Vulkan loader or a GPU (CI runners, this
//! sandbox) the spike reports the structured init result and exits 0. With a
//! device present it creates an instance with the Khronos validation layer when
//! available, a logical device with a compute queue, and the buffer/fence
//! objects the workload would need — demonstrating manual resource-lifetime
//! and synchronization management. Per review feedback, this spike executes
//! NO compute and NO rendering: it demonstrates initialization and resource
//! management only (shaders require precompiled SPIR-V via an external
//! toolchain, not exercised here).
//!
//! Every ash call is `unsafe`; this file contains no safe wrapper. A small RAII
//! guard destroys the instance/device/buffers/fence on every path, including
//! early error returns, because resource lifetime is one of the comparison
//! criteria.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

const INSTANCED_MARKERS: usize = 10_000;
const INSTANCE_BUFFER_BYTES: usize = INSTANCED_MARKERS * 16;
const COMPUTE_BUFFER_BYTES: usize = 4096;

/// Owns the Vulkan objects created by the spike and destroys them in reverse
/// order on drop, so every early return cleans up (manual lifetime management
/// made explicit — and contained — via RAII).
struct VulkanGuard {
    instance: ash::Instance,
    device: Option<ash::Device>,
    buffers: Vec<ash::vk::Buffer>,
    fence: Option<ash::vk::Fence>,
}

impl VulkanGuard {
    fn new(instance: ash::Instance) -> Self {
        Self {
            instance,
            device: None,
            buffers: Vec::new(),
            fence: None,
        }
    }
}

impl Drop for VulkanGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(device) = self.device.take() {
                for buffer in self.buffers.drain(..) {
                    device.destroy_buffer(buffer, None);
                }
                if let Some(fence) = self.fence.take() {
                    device.destroy_fence(fence, None);
                }
                device.destroy_device(None);
            }
            self.instance.destroy_instance(None);
        }
    }
}

fn print_planned_workload() {
    println!(
        "WORKLOAD candidate=ash instanced_markers={INSTANCED_MARKERS} instance_buffer_bytes={INSTANCE_BUFFER_BYTES} \
         terrain_mesh=grid draw_calls_planned=1 compute_buffer_bytes={COMPUTE_BUFFER_BYTES} \
         note=\"planned only: this spike executes no compute and no rendering; it demonstrates init and resource/lifetime management\""
    );
}

fn main() {
    println!("INIT candidate=ash version=0.38.0+1.3.281 api=vulkan unsafe_api=100%");

    let entry = match unsafe { ash::Entry::load() } {
        Ok(entry) => entry,
        Err(error) => {
            println!(
                "ADAPTER candidate=ash found=false error=\"vulkan loader unavailable: {error}\""
            );
            print_planned_workload();
            return;
        }
    };

    let version = unsafe { entry.try_enumerate_instance_version() }
        .ok()
        .flatten()
        .map(|v| format!("{}.{}.{}", v >> 22, (v >> 12) & 0x3ff, v & 0xfff));
    println!("INSTANCE candidate=ash vulkan_loader_version={version:?}");

    let layers = unsafe { entry.enumerate_instance_layer_properties() }.unwrap_or_default();
    let validation_available = layers.iter().any(|layer| {
        let name = unsafe { CStr::from_ptr(layer.layer_name.as_ptr()) };
        name == c"VK_LAYER_KHRONOS_validation"
    });
    println!("VALIDATION candidate=ash layer=VK_LAYER_KHRONOS_validation available={validation_available}");

    let app_name = CString::new("phalanx-graphics-spike").expect("static C string");
    let engine_name = CString::new("phalanx").expect("static C string");
    let app_info = ash::vk::ApplicationInfo {
        p_application_name: app_name.as_ptr(),
        application_version: ash::vk::make_api_version(0, 1, 0, 0),
        p_engine_name: engine_name.as_ptr(),
        engine_version: ash::vk::make_api_version(0, 1, 0, 0),
        api_version: ash::vk::API_VERSION_1_3,
        ..Default::default()
    };
    let layer_names: Vec<CString> = if validation_available {
        vec![CString::new("VK_LAYER_KHRONOS_validation").expect("static C string")]
    } else {
        Vec::new()
    };
    let layer_ptrs: Vec<*const c_char> = layer_names.iter().map(|name| name.as_ptr()).collect();
    let instance_create_info = ash::vk::InstanceCreateInfo {
        p_application_info: &app_info,
        enabled_layer_count: layer_ptrs.len() as u32,
        pp_enabled_layer_names: layer_ptrs.as_ptr(),
        ..Default::default()
    };
    let instance = match unsafe { entry.create_instance(&instance_create_info, None) } {
        Ok(instance) => instance,
        Err(error) => {
            println!("INSTANCE candidate=ash created=false error=\"{error:?}\"");
            print_planned_workload();
            return;
        }
    };
    // From here on the guard owns the instance and destroys it on every path.
    let mut guard = VulkanGuard::new(instance);
    println!(
        "INSTANCE candidate=ash created=true validation_layers={}",
        layer_ptrs.len()
    );

    let physical_devices = match unsafe { guard.instance.enumerate_physical_devices() } {
        Ok(devices) => devices,
        Err(error) => {
            println!("ADAPTER candidate=ash found=false error=\"{error:?}\"");
            print_planned_workload();
            return;
        }
    };
    if physical_devices.is_empty() {
        println!("ADAPTER candidate=ash found=false error=\"no physical devices (no GPU / no ICD in this environment)\"");
        print_planned_workload();
        return;
    }

    let physical = physical_devices[0];
    let props = unsafe { guard.instance.get_physical_device_properties(physical) };
    let device_name = unsafe { CStr::from_ptr(props.device_name.as_ptr()) }
        .to_string_lossy()
        .into_owned();
    println!(
        "ADAPTER candidate=ash found=true name=\"{device_name}\" type={:?} api={}.{}.{}",
        props.device_type,
        props.api_version >> 22,
        (props.api_version >> 12) & 0x3ff,
        props.api_version & 0xfff
    );

    // Require a compute-capable queue family; no silent fallback to family 0.
    let queue_families = unsafe {
        guard
            .instance
            .get_physical_device_queue_family_properties(physical)
    };
    let Some(queue_family) = queue_families
        .iter()
        .position(|family| family.queue_flags.contains(ash::vk::QueueFlags::COMPUTE))
        .map(|index| index as u32)
    else {
        println!("DEVICE candidate=ash created=false error=\"no compute-capable queue family on the selected physical device\"");
        print_planned_workload();
        return;
    };

    let queue_priority = 1.0f32;
    let device_queue_create_info = ash::vk::DeviceQueueCreateInfo {
        queue_family_index: queue_family,
        queue_count: 1,
        p_queue_priorities: &queue_priority,
        ..Default::default()
    };
    let device_create_info = ash::vk::DeviceCreateInfo {
        queue_create_info_count: 1,
        p_queue_create_infos: &device_queue_create_info,
        ..Default::default()
    };
    let device = match unsafe {
        guard
            .instance
            .create_device(physical, &device_create_info, None)
    } {
        Ok(device) => device,
        Err(error) => {
            println!("DEVICE candidate=ash created=false error=\"{error:?}\"");
            print_planned_workload();
            return;
        }
    };
    guard.device = Some(device);
    let device = guard.device.as_ref().expect("device just set");
    println!("DEVICE candidate=ash created=true queue_family={queue_family} compute_queue=true");

    // Workload resources: instance buffer for 10k markers + compute buffer, plus
    // the fence a frame loop must manage manually (synchronization is the
    // caller's responsibility in raw Vulkan).
    let compute_buffer_info = ash::vk::BufferCreateInfo {
        size: COMPUTE_BUFFER_BYTES as u64,
        usage: ash::vk::BufferUsageFlags::STORAGE_BUFFER | ash::vk::BufferUsageFlags::TRANSFER_DST,
        sharing_mode: ash::vk::SharingMode::EXCLUSIVE,
        ..Default::default()
    };
    let instance_buffer_info = ash::vk::BufferCreateInfo {
        size: INSTANCE_BUFFER_BYTES as u64,
        usage: ash::vk::BufferUsageFlags::VERTEX_BUFFER | ash::vk::BufferUsageFlags::TRANSFER_DST,
        sharing_mode: ash::vk::SharingMode::EXCLUSIVE,
        ..Default::default()
    };
    let compute_buffer =
        unsafe { device.create_buffer(&compute_buffer_info, None) }.expect("buffer");
    let instance_buffer =
        unsafe { device.create_buffer(&instance_buffer_info, None) }.expect("buffer");
    guard.buffers.push(compute_buffer);
    guard.buffers.push(instance_buffer);
    let fence =
        unsafe { device.create_fence(&ash::vk::FenceCreateInfo::default(), None) }.expect("fence");
    guard.fence = Some(fence);
    println!(
        "RESOURCES candidate=ash instance_buffer_bytes={INSTANCE_BUFFER_BYTES} compute_buffer_bytes={COMPUTE_BUFFER_BYTES} \
         sync=\"manual: fences/events, pipeline barriers, and memory lifetimes are caller-managed (here contained in a Drop guard)\""
    );
    println!("SHADER candidate=ash language=spir-v note=\"precompiled SPIR-V required; external toolchain (glslangValidator/shaderc), not exercised in this spike\"");

    print_planned_workload();
}
