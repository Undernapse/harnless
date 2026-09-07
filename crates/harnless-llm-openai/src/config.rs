//! Configuration for the OpenAI-compatible adapter.
//!
//! The adapter is a swappable provider: it targets any endpoint speaking the
//! OpenAI chat-completions streaming interface (DeepSeek, vLLM, and friends).
//! All knobs live here so swapping providers never touches consumers.

use std::time::Duration;

/// Configuration for an [`OpenAiAdapter`](crate::OpenAiAdapter).
#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    /// Base URL of the OpenAI-compatible endpoint, including the API prefix
    /// (e.g. `https://api.deepseek.com/v1` or `https://host:port/v1`).
    pub base_url: String,
    /// The API key, if the endpoint requires one. Supplied by credentials at
    /// boot; the adapter itself never resolves secrets.
    pub api_key: Option<String>,
    /// The model identifier sent as `model` in each request.
    pub model: String,
    /// The declared application identity sent on every request (adapter
    /// obligation): a stable attribution string so providers can attribute
    /// traffic without leaking anything per-user.
    pub identity: String,
    /// Max time to wait between streamed bytes before declaring a stall.
    /// A hung provider surfaces as a timeout, never a frozen session.
    pub idle_timeout: Duration,
    /// Max time for the connection and request headers.
    pub request_timeout: Duration,
}

impl OpenAiConfig {
    /// Build a config from the required fields, with conservative defaults
    /// for the two watchdog timeouts.
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        identity: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            api_key,
            model: model.into(),
            identity: identity.into(),
            idle_timeout: Duration::from_secs(60),
            request_timeout: Duration::from_secs(30),
        }
    }

    /// Set the idle (stall) watchdog timeout.
    pub fn idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Set the connection/request timeout.
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// The chat-completions URL for this base URL.
    pub fn chat_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_url_joins_without_double_slash() {
        let cfg = OpenAiConfig::new("https://api.deepseek.com/v1", None, "m", "id");
        assert_eq!(cfg.chat_url(), "https://api.deepseek.com/v1/chat/completions");
        let cfg = OpenAiConfig::new("https://host/v1/", None, "m", "id");
        assert_eq!(cfg.chat_url(), "https://host/v1/chat/completions");
    }
}
