//! A client for a model server on this machine that speaks the OpenAI
//! chat-completions API, such as Ollama or llama-server.

mod client;
mod error;
mod stream;

pub use client::Client;
pub use error::ProviderError;
