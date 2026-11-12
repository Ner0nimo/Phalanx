//! Issue #10 spike: ash (raw Vulkan bindings) renderer-API path (input to
//! ADR-0004).
//!
//! Headless by design: without the Vulkan loader or a GPU (CI runners, this
//! sandbox) the spike reports the structured init result and the planned
//! workload, and exits 0. With a device present it creates an instance with
//! the Khronos validation layer when available, a logical device with a
//! compute queue, and the buffer/fence objects the workload needs, making the
//! manual resource-lifetime and synchronization responsibilities explicit.
//!
//! Every ash call is `unsafe`; this file contains no safe wrapper. Shaders for
//! ash must be precompiled SPIR-V (external toolchain: glslangValidator or
//! shaderc), which is not exercised in this headless spike.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

const INSTANCED_MARKERS: usize = 10_000;
const INSTANCE_BUFFER_BYTES: usize = INSTANCED_MARKERS * 16;
const COMPUTE_BUFFER_BYTES: usize = 4096;

fn print_planned_workload() {
    println!(
        "WORKLOAD candidate=ash instanced_markers={INSTANCED_MARKERS} instance_buffer_bytes={INSTANCE_BUFFER_BYTES} \
         terrain_mesh=grid draw_calls_planned=1 compute_buffer_bytes={COMPUTE_BUFFER_BYTES} \
         note=\"planned; executed only when a GPU adapter is present\""
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
    println!(
        "INSTANCE candidate=ash created=true validation_layers={}",
        layer_ptrs.len()
    );

    let physical_devices = match unsafe { instance.enumerate_physical_devices() } {
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
        unsafe { instance.destroy_instance(None) };
        return;
    }

    let physical = physical_devices[0];
    let props = unsafe { instance.get_physical_device_properties(physical) };
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

    // Find a queue family with compute support.
    let queue_families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
    let queue_family = queue_families
        .iter()
        .enumerate()
        .find(|(_, family)| family.queue_flags.contains(ash::vk::QueueFlags::COMPUTE))
        .map(|(index, _)| index as u32)
        .unwrap_or(0);

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
    let device = match unsafe { instance.create_device(physical, &device_create_info, None) } {
        Ok(device) => device,
        Err(error) => {
            println!("DEVICE candidate=ash created=false error=\"{error:?}\"");
            print_planned_workload();
            return;
        }
    };
    println!("DEVICE candidate=ash created=true queue_family={queue_family} compute_queue=true");

    // Workload resources: instance buffer for 10k markers + compute buffer, plus
    // the fence the frame loop must manage manually (synchronization is the
    // caller's responsibility in raw Vulkan).
    let buffer_create_info = ash::vk::BufferCreateInfo {
        size: COMPUTE_BUFFER_BYTES as u64,
        usage: ash::vk::BufferUsageFlags::STORAGE_BUFFER | ash::vk::BufferUsageFlags::TRANSFER_DST,
        sharing_mode: ash::vk::SharingMode::EXCLUSIVE,
        ..Default::default()
    };
    let compute_buffer =
        unsafe { device.create_buffer(&buffer_create_info, None) }.expect("buffer");
    let instance_buffer_info = ash::vk::BufferCreateInfo {
        size: INSTANCE_BUFFER_BYTES as u64,
        usage: ash::vk::BufferUsageFlags::VERTEX_BUFFER | ash::vk::BufferUsageFlags::TRANSFER_DST,
        sharing_mode: ash::vk::SharingMode::EXCLUSIVE,
        ..Default::default()
    };
    let instance_buffer =
        unsafe { device.create_buffer(&instance_buffer_info, None) }.expect("buffer");
    let fence_create_info = ash::vk::FenceCreateInfo::default();
    let fence = unsafe { device.create_fence(&fence_create_info, None) }.expect("fence");
    println!(
        "RESOURCES candidate=ash instance_buffer_bytes={INSTANCE_BUFFER_BYTES} compute_buffer_bytes={COMPUTE_BUFFER_BYTES} \
         sync=\"manual: fences/events, pipeline barriers, and memory lifetimes are caller-managed\""
    );
    println!("SHADER candidate=ash language=spir-v note=\"precompiled SPIR-V required; external toolchain (glslangValidator/shaderc), not exercised headless\"");

    unsafe {
        device.destroy_buffer(compute_buffer, None);
        device.destroy_buffer(instance_buffer, None);
        device.destroy_fence(fence, None);
        device.destroy_device(None);
        instance.destroy_instance(None);
    }

    print_planned_workload();
}
