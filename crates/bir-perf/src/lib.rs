//! `bir-perf` — keeping the browser small and responsive.
//!
//! A webview is not free. Each one carries a renderer with its own DOM, JS heap,
//! compositor surfaces and codec state; a browser that keeps every tab alive is a
//! browser that uses gigabytes. This crate is where the browser decides *what deserves
//! memory*, and it is the part that most separates a real browser from a demo.
//!
//! Three mechanisms, cheapest first:
//!
//! **Lazy creation.** A restored or background tab is a record, not a webview. Nothing
//! is allocated until the tab is first shown. Session restore of 200 tabs costs a few
//! hundred kilobytes instead of a few gigabytes.
//!
//! **Sleeping.** A hidden webview is unmapped (the platform stops compositing it) and,
//! where the platform supports it, its task queues are suspended. State is intact, so
//! waking is instantaneous.
//!
//! **Discarding.** The webview is dropped entirely and the memory is returned to the
//! OS. The tab keeps its title, URL, favicon and scroll-free placeholder; activating it
//! reloads. This is what Chrome and Firefox do, and it is the only mechanism that
//! actually gives memory back under pressure.

pub mod gpu;
pub mod lifecycle;
pub mod memory;

pub use gpu::{apply_gpu_policy, gpu_report, webview2_extra_args, GpuInfo};
pub use lifecycle::{
  LifecycleAction, LifecyclePolicy, LifecycleScheduler, TabActivity, TabLifecycle,
  ESTIMATED_WEBVIEW_MIB,
};
pub use memory::{
  process_cpu_ticks, CpuSampler, MemorySampler, MemorySnapshot, Pressure,
};
