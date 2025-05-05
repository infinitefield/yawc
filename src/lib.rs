#[doc(hidden)]
#[cfg(target_arch = "wasm32")]
mod wasm;

#[doc(hidden)]
#[cfg(not(target_arch = "wasm32"))]
mod native;

#[cfg(not(target_arch = "wasm32"))]
mod compression;

#[cfg(not(target_arch = "wasm32"))]
pub mod close;
#[cfg(not(target_arch = "wasm32"))]
pub mod codec;
#[cfg(not(target_arch = "wasm32"))]
pub mod frame;
#[cfg(not(target_arch = "wasm32"))]
mod mask;
#[cfg(not(target_arch = "wasm32"))]
mod stream;

#[cfg(target_arch = "wasm32")]
pub use wasm::*;

#[cfg(not(target_arch = "wasm32"))]
pub use native::*;
