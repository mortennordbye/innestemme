//! Where the candle models (Whisper) run.

use anyhow::{bail, Result};
use candle::Device;

/// The Apple GPU when asked for and compiled in (`metal` feature), else the CPU.
pub fn device(gpu: bool) -> Result<Device> {
    if !gpu {
        return Ok(Device::Cpu);
    }
    if !candle::utils::metal_is_available() {
        bail!("no GPU backend: build with `--features metal` on a Mac");
    }
    Ok(Device::new_metal(0)?)
}
