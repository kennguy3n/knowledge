//! Concrete [`crate::InferenceAdapter`] implementations.

pub mod fallback;
pub mod llama_cpp;
pub mod mlx;

pub use fallback::FallbackAdapter;
#[cfg(feature = "http-client")]
pub use llama_cpp::HttpLlamaServerClient;
pub use llama_cpp::{LlamaCppAdapter, LlamaServerClient};
#[cfg(any(test, feature = "test-support"))]
pub use mlx::clear_mlx_generate_fn;
pub use mlx::{
    get_mlx_generate_fn, set_mlx_generate_fn, set_mlx_runtime_linked, MlxAdapter, MlxGenerateFn,
};
