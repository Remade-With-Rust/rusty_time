//! rusty_time-alloc — the allocator seam.
//!
//! Deliverable crates (and only deliverables) declare:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: rusty_time_alloc::HouseAllocator = rusty_time_alloc::house_allocator();
//! ```
//!
//! Libraries never touch this crate, and never depend on `rusty_alloc-api`
//! directly — the pin to the house allocator lives here, once, so swapping it
//! is a one-line change for every deliverable at once (mission plan §4).

pub use rusty_alloc_api::RustyAlloc as HouseAllocator;

pub const fn house_allocator() -> HouseAllocator {
    rusty_alloc_api::RustyAlloc
}
