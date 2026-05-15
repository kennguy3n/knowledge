//! Concrete [`crate::InferenceAdapter`] implementations.

pub mod fallback;
pub mod llama_cpp;
pub mod mlx;

pub use fallback::FallbackAdapter;
#[cfg(feature = "http-client")]
pub use llama_cpp::HttpLlamaServerClient;
pub use llama_cpp::{LlamaCppAdapter, LlamaServerClient};
pub use mlx::MlxAdapter;
