//! X11 backend. See the agent task.
use anyhow::{bail, Result};
pub fn open(_title: &str) -> Result<Box<dyn super::Backend>> {
    bail!("x11 backend not written yet")
}
