//! The virtio-MMIO transport and its virtqueues.

mod mmio;
mod queue;

pub use mmio::Transport;
pub use queue::{Buffer, Chain, Queue, QueueError};
