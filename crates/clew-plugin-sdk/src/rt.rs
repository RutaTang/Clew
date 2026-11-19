//! The three things the generated bindings ([`crate::bindings`]) call on
//! their runtime, written out here so that building a plugin needs no crate
//! but this one.
//!
//! They follow `wit-bindgen`'s own runtime (`wit-bindgen` 0.57.1,
//! `src/rt/mod.rs`, Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT).
//! That crate ships a build script and a prebuilt object for
//! `cabi_realloc`, the allocator the host calls to hand a plugin its
//! arguments. On `wasm32-wasip2`, the only target Clew builds plugins for,
//! Rust's standard library exports `cabi_realloc` itself, so neither is
//! needed: [`maybe_link_cabi_realloc`] has nothing to do.

extern crate alloc;

use alloc::alloc::Layout;
use core::ptr::{self, NonNull};

/// Make sure `cabi_realloc` reaches the linker. On `wasm32-wasip2` the
/// standard library exports it already.
pub fn maybe_link_cabi_realloc() {}

/// Run the module's static constructors, once, before the first export does
/// anything: called at the start of every export.
#[cfg(target_arch = "wasm32")]
pub fn run_ctors_once() {
    static mut RUN: bool = false;
    // SAFETY: a plugin's instance runs on one thread, and `wasm-ld`
    // provides `__wasm_call_ctors` to every module it links.
    unsafe {
        if !RUN {
            unsafe extern "C" {
                fn __wasm_call_ctors();
            }
            __wasm_call_ctors();
            RUN = true;
        }
    }
}

/// Memory the bindings allocate to hand a list or a string to the host,
/// freed when the call returns.
pub struct Cleanup {
    ptr: NonNull<u8>,
    layout: Layout,
}

// SAFETY: it only owns an allocation, which any thread may free.
unsafe impl Send for Cleanup {}
// SAFETY: as for `Send`; it has no shared state.
unsafe impl Sync for Cleanup {}

impl Cleanup {
    /// Allocate `layout`: a null pointer and nothing to free for a size of
    /// zero.
    pub fn new(layout: Layout) -> (*mut u8, Option<Cleanup>) {
        if layout.size() == 0 {
            return (ptr::null_mut(), None);
        }
        // SAFETY: the size is not zero.
        let ptr = unsafe { alloc::alloc::alloc(layout) };
        let Some(ptr) = NonNull::new(ptr) else {
            alloc::alloc::handle_alloc_error(layout)
        };
        (ptr.as_ptr(), Some(Cleanup { ptr, layout }))
    }

    /// Leave the memory allocated: its owner is now someone else.
    pub fn forget(self) {
        core::mem::forget(self);
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout, and freed once.
        unsafe { alloc::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}
