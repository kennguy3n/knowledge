//! Concrete [`crate::InferenceAdapter`] implementations.

// Shared core for the NPU/accelerator adapters — compiled whenever at
// least one accelerator adapter is enabled.
#[cfg(any(feature = "coreml", feature = "onnx-runtime"))]
pub mod accelerator;
#[cfg(feature = "coreml")]
pub mod coreml;
pub mod fallback;
pub mod llama_cpp;
pub mod managed_cloud;
pub mod mlx;
#[cfg(feature = "onnx-runtime")]
pub mod onnx_runtime;

#[cfg(feature = "coreml")]
pub use coreml::{CoreMl, CoreMlAdapter};
#[cfg(feature = "onnx-runtime")]
pub use onnx_runtime::{OnnxRuntime, OnnxRuntimeAdapter};

pub use fallback::FallbackAdapter;
#[cfg(feature = "http-client")]
pub use llama_cpp::HttpLlamaServerClient;
pub use llama_cpp::{LlamaCppAdapter, LlamaServerClient};
#[cfg(feature = "http-client")]
pub use managed_cloud::HttpManagedInferenceClient;
pub use managed_cloud::{ManagedCloudAdapter, ManagedInferenceClient};
#[cfg(any(test, feature = "test-support"))]
pub use mlx::{clear_mlx_generate_fn, clear_mlx_generate_with_sampling_fn};
pub use mlx::{
    get_mlx_generate_fn, get_mlx_generate_with_sampling_fn, set_mlx_generate_fn,
    set_mlx_generate_with_sampling_fn, set_mlx_runtime_linked, MlxAdapter, MlxGenerateFn,
    MlxGenerateWithSamplingFn,
};
