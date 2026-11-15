//! Test build for #2970: after a GPU device loss, ask the Vulkan driver
//! itself whether it lost the device too. wgpu loses the device on its own
//! when a call answers "out of memory" (wgpu-core `handle_hal_error`), as
//! the swapchain acquire does on RADV when Mesa's WSI fails a sync-file or
//! syncobj ioctl; the driver may still run the device then. A real reset
//! of the card makes RADV answer `VK_ERROR_DEVICE_LOST`.
//!
//! The answer is one fixed line, which the crash report reads
//! (aurora-viewer crash/summary.rs, "Driver's answer after the loss").

use std::time::Duration;

/// How long the driver is given: a card still hung answers only at the
/// kernel's timeout.
const WAIT: Duration = Duration::from_secs(3);

/// The fixed answers (the report publishes only these).
pub const ANSWERS: [&str; 5] = [
    "the driver still runs the device (VK_SUCCESS)",
    "the driver lost the device too (VK_ERROR_DEVICE_LOST)",
    "the driver gave another answer",
    "no answer within 3 s (the card may be hung)",
    "not a Vulkan device",
];

/// Logs the driver's answer once (`GPU loss probe: …`). Runs on its own
/// thread: `vkDeviceWaitIdle` can block on a hung card.
pub fn probe(device: &wgpu::Device) {
    let device = device.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new().name("gpu-loss-probe".into()).spawn(move || {
        let _ = tx.send(answer(&device));
    });
    let text = match spawned {
        Ok(_) => rx.recv_timeout(WAIT).unwrap_or(ANSWERS[3]),
        Err(_) => ANSWERS[2],
    };
    log::error!("GPU loss probe: {text}");
}

#[cfg(not(target_os = "macos"))]
fn answer(device: &wgpu::Device) -> &'static str {
    // SAFETY: the hal device is only borrowed for this call; waiting for it
    // to be idle reads its state and changes nothing.
    let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else {
        return ANSWERS[4];
    };
    // SAFETY: as above.
    match unsafe { hal.raw_device().device_wait_idle() } {
        Ok(()) => ANSWERS[0],
        Err(ash::vk::Result::ERROR_DEVICE_LOST) => ANSWERS[1],
        Err(other) => {
            log::warn!("GPU loss probe: vkDeviceWaitIdle answered {other:?}");
            ANSWERS[2]
        }
    }
}

#[cfg(target_os = "macos")]
fn answer(_device: &wgpu::Device) -> &'static str {
    ANSWERS[4]
}

/// Window images the driver refused (an "out of memory" answer of
/// `vkAcquireNextImageKHR`) that the test wgpu took as a lost surface, the
/// swapchain rebuilt instead of the device lost.
#[cfg(not(target_os = "macos"))]
pub fn acquire_refusals() -> u32 {
    wgpu::hal::vulkan::acquire_refusal::total()
}

#[cfg(target_os = "macos")]
pub fn acquire_refusals() -> u32 {
    0
}
