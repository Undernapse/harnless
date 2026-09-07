//! # harnless-llm-openai
//!
//! OpenAI-compatible model adapter for harnless: streaming SSE against any
//! endpoint speaking the chat-completions interface (DeepSeek, vLLM, and
//! friends). Implements the [`ModelAdapter`] seam from `harnless-seams`;
//! register it as a provider to make any such endpoint usable without
//! touching the loop or a consumer.
//!
//! Adapter obligations honored here (conformance contract 05): declared
//! identity header, raw-JSON tool arguments end to end, two sanctioned
//! failure paths, one attempt per call, a transport watchdog, canonical
//! context-overflow classification, empty completion as a retryable failure,
//! and disjoint usage accounting.

mod adapter;
mod request;
mod sse;

pub use adapter::OpenAiAdapter;
pub use config::OpenAiConfig;

mod config;
