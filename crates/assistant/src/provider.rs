//! Which provider answers, with which model and at which address.
use jykre_core::{DriverError, DriverResult};
use serde::{Deserialize, Serialize};

/// A provider family. Each speaks its own wire protocol; any server that
/// speaks OpenAI's chat completions (LM Studio, a Databricks serving
/// endpoint, a gateway) is `OpenAiCompatible`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    #[serde(rename = "openai")]
    OpenAi,
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible,
    Ollama,
}

/// Whether a provider is called with an API key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyUse {
    Required,
    /// Local or self-hosted servers often take none.
    Optional,
    None,
}

impl ProviderKind {
    pub const ALL: [Self; 4] = [
        Self::Anthropic,
        Self::OpenAi,
        Self::OpenAiCompatible,
        Self::Ollama,
    ];

    /// The stable name used in preferences and in the vault account.
    pub fn key(self) -> &'static str {
        match self {
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::OpenAiCompatible => "openai-compatible",
            Self::Ollama => "ollama",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    pub fn key_use(self) -> KeyUse {
        match self {
            Self::Anthropic | Self::OpenAi => KeyUse::Required,
            Self::OpenAiCompatible => KeyUse::Optional,
            Self::Ollama => KeyUse::None,
        }
    }

    /// The address used when the preferences give none. An OpenAI-compatible
    /// server has no default: its address is the whole point of choosing it.
    pub fn default_endpoint(self) -> Option<&'static str> {
        match self {
            Self::Anthropic => Some("https://api.anthropic.com/v1/"),
            Self::OpenAi => Some("https://api.openai.com/v1/"),
            Self::OpenAiCompatible => None,
            Self::Ollama => Some("http://localhost:11434/"),
        }
    }
}

/// A provider, a model and, when needed, an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    /// The provider's own model name, typed by the user; no model names are
    /// built into the application.
    pub model: String,
    /// `None` uses [`ProviderKind::default_endpoint`].
    pub endpoint: Option<String>,
}

impl ProviderConfig {
    /// The address requests go to, always ending in `/`.
    pub fn endpoint(&self) -> DriverResult<String> {
        let endpoint = self
            .endpoint
            .as_deref()
            .map(str::trim)
            .filter(|endpoint| !endpoint.is_empty())
            .or(self.kind.default_endpoint())
            .ok_or_else(|| DriverError::validation("Enter the address of the AI server"))?;
        Ok(if endpoint.ends_with('/') {
            endpoint.to_owned()
        } else {
            format!("{endpoint}/")
        })
    }

    /// A model is named, and the address is HTTPS unless it is this
    /// computer: plain HTTP is accepted only for loopback, where local
    /// servers such as Ollama listen. An address never carries credentials.
    pub fn validate(&self) -> DriverResult<()> {
        if self.model.trim().is_empty() {
            return Err(DriverError::validation("Enter the model name"));
        }
        let endpoint = self.endpoint()?;
        let url = reqwest::Url::parse(&endpoint)
            .map_err(|_| DriverError::validation("The AI server address is not a valid URL"))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(DriverError::validation(
                "The AI server address must not contain a user or password",
            ));
        }
        if url.query().is_some() || url.fragment().is_some() {
            return Err(DriverError::validation(
                "The AI server address must not contain a query or fragment",
            ));
        }
        match url.scheme() {
            "https" => Ok(()),
            "http" if is_loopback(&url) => Ok(()),
            "http" => Err(DriverError::validation(
                "Use HTTPS for an AI server that is not on this computer",
            )),
            _ => Err(DriverError::validation(
                "The AI server address must start with https://",
            )),
        }
    }
}

fn is_loopback(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(kind: ProviderKind, endpoint: Option<&str>) -> ProviderConfig {
        ProviderConfig {
            kind,
            model: "model".into(),
            endpoint: endpoint.map(Into::into),
        }
    }

    #[test]
    fn keys_round_trip_and_defaults_end_with_a_slash() {
        for kind in ProviderKind::ALL {
            assert_eq!(ProviderKind::from_key(kind.key()), Some(kind));
            if let Some(endpoint) = kind.default_endpoint() {
                assert!(endpoint.ends_with('/'));
            }
        }
        assert_eq!(
            config(
                ProviderKind::OpenAiCompatible,
                Some("https://gw.example/v1")
            )
            .endpoint()
            .unwrap(),
            "https://gw.example/v1/"
        );
    }

    #[test]
    fn only_https_or_loopback_http_is_accepted() {
        for ok in [
            "https://gw.example/v1/",
            "http://localhost:1234/v1/",
            "http://127.0.0.1:11434/",
            "http://[::1]:8080/",
        ] {
            assert!(
                config(ProviderKind::OpenAiCompatible, Some(ok))
                    .validate()
                    .is_ok(),
                "{ok}"
            );
        }
        for bad in [
            "http://gw.example/v1/",
            "http://192.168.0.10:1234/",
            "ftp://gw.example/",
            "https://user:pass@gw.example/",
            "https://gw.example/v1/?key=1",
            "not a url",
        ] {
            assert!(
                config(ProviderKind::OpenAiCompatible, Some(bad))
                    .validate()
                    .is_err(),
                "{bad}"
            );
        }
        assert!(
            config(ProviderKind::OpenAiCompatible, None)
                .validate()
                .is_err()
        );
        assert!(config(ProviderKind::Ollama, None).validate().is_ok());
        let mut unnamed = config(ProviderKind::OpenAi, None);
        unnamed.model = "  ".into();
        assert!(unnamed.validate().is_err());
    }
}
