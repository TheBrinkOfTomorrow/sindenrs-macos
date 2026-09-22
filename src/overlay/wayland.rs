//! Wayland backend. See the agent task.
use anyhow::{bail, Result};
pub fn open(_title: &str) -> Result<Box<dyn super::Backend>> {
    bail!("wayland backend not written yet")
}
