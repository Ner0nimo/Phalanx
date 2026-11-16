//! Headless-capable wgpu backend initialization and adapter reporting
//! (issue #61).
//!
//! [`init_headless`] creates a wgpu instance with validation enabled,
//! requests a real hardware adapter (no fallback), and creates a device +
//! queue — no surface, no window, so it runs on headless machines. Without a
//! usable adapter it returns the structured [`BackendError::NoAdapter`]
//! (the CI probe convention from the graphics spike: structured result,
//! never a panic).

use std::fmt;

/// Structured errors from GPU backend initialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BackendError {
    /// No suitable GPU adapter exists on this machine (headless CI, missing
    /// drivers). The string is the adapter request error.
    NoAdapter(String),
    /// An adapter was found but device creation failed.
    Device(String),
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAdapter(error) => write!(formatter, "no suitable GPU adapter: {error}"),
            Self::Device(error) => write!(formatter, "GPU device creation failed: {error}"),
        }
    }
}

impl std::error::Error for BackendError {}

/// The startup adapter report logged when a device is created (issue #61:
/// "adapter name, backend and device information are logged at startup", plus
/// the OS/build context required by the #33 hardware-validation record).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterReport {
    /// Adapter name (e.g. the GPU model).
    pub name: String,
    /// PCI vendor ID.
    pub vendor: u32,
    /// PCI device ID.
    pub device: u32,
    /// Adapter type (discrete/integrated/software/…), rendered as text.
    pub device_type: String,
    /// wgpu backend (Vulkan/Metal/DX12/GL).
    pub backend: String,
    /// Driver name.
    pub driver: String,
    /// Driver version/info string.
    pub driver_info: String,
    /// Host OS (`std::env::consts::OS`).
    pub os: &'static str,
    /// Host architecture.
    pub arch: &'static str,
    /// Build profile (`debug`/`release`).
    pub profile: &'static str,
}

impl fmt::Display for AdapterReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "ADAPTER candidate=phalanx_render found=true name=\"{}\" vendor={:#x} device={:#x} type={} backend={} driver=\"{}\" driver_info=\"{}\" os={} arch={} profile={} validation=on",
            self.name,
            self.vendor,
            self.device,
            self.device_type,
            self.backend,
            self.driver,
            self.driver_info,
            self.os,
            self.arch,
            self.profile
        )
    }
}

/// An initialized GPU backend: instance, adapter, device, queue.
pub struct GpuBackend {
    /// The wgpu instance.
    pub instance: wgpu::Instance,
    /// The selected hardware adapter.
    pub adapter: wgpu::Adapter,
    /// The logical device.
    pub device: wgpu::Device,
    /// The command queue.
    pub queue: wgpu::Queue,
}

/// Creates the instance descriptor used everywhere in Phalanx: all backends,
/// validation enabled, no display handle (surfaces supply their own).
pub fn instance_descriptor() -> wgpu::InstanceDescriptor {
    let mut descriptor = wgpu::InstanceDescriptor::new_without_display_handle();
    descriptor.backends = wgpu::Backends::all();
    descriptor.flags = wgpu::InstanceFlags::VALIDATION;
    descriptor
}

/// Initializes a headless GPU backend (no surface): instance with validation,
/// a real hardware adapter (fallback adapters are NOT forced), device, queue.
///
/// Returns [`BackendError::NoAdapter`] with the structured reason when the
/// machine has no usable GPU — headless CI gets a verifiable probe result.
pub fn init_headless() -> Result<(GpuBackend, AdapterReport), BackendError> {
    let instance = wgpu::Instance::new(instance_descriptor());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
        apply_limit_buckets: false,
    }))
    .map_err(|error| BackendError::NoAdapter(error.to_string()))?;
    init_from_adapter(instance, adapter)
}

/// Shared device-creation path for headless and surface adapters.
pub(crate) fn init_from_adapter(
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
) -> Result<(GpuBackend, AdapterReport), BackendError> {
    let info = adapter.get_info();
    let report = AdapterReport {
        name: info.name.clone(),
        vendor: info.vendor,
        device: info.device,
        device_type: format!("{:?}", info.device_type),
        backend: format!("{:?}", info.backend),
        driver: info.driver.clone(),
        driver_info: info.driver_info.clone(),
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        profile: if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        },
    };
    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("phalanx-render"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::default(),
        ..Default::default()
    }))
    .map_err(|error| BackendError::Device(error.to_string()))?;
    Ok((
        GpuBackend {
            instance,
            adapter,
            device,
            queue,
        },
        report,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_adapter_is_a_structured_error_not_a_panic() {
        // In this sandbox (and hosted CI) there is no GPU adapter; on a GPU
        // machine the backend initializes. Both outcomes are asserted.
        match init_headless() {
            Err(BackendError::NoAdapter(message)) => {
                assert!(!message.is_empty(), "the reason must be structured");
            }
            Err(BackendError::Device(message)) => {
                panic!("adapter found but device failed: {message}");
            }
            Ok((backend, report)) => {
                println!("{report}");
                assert!(!report.name.is_empty());
                assert!(!report.backend.is_empty());
                // The device must be usable: a zero-size validation query.
                let _ = backend.device.limits();
            }
        }
    }

    #[test]
    fn report_display_is_a_single_structured_line() {
        let report = AdapterReport {
            name: "Test GPU".to_owned(),
            vendor: 0x10de,
            device: 0x2684,
            device_type: "DiscreteGpu".to_owned(),
            backend: "Vulkan".to_owned(),
            driver: "nvidia".to_owned(),
            driver_info: "550.0".to_owned(),
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            profile: "release",
        };
        let line = report.to_string();
        assert!(line.starts_with("ADAPTER candidate=phalanx_render found=true"));
        assert!(line.contains("name=\"Test GPU\""));
        assert!(line.contains("backend=Vulkan"));
        assert!(!line.contains('\n'));
    }
}
