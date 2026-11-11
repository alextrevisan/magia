use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use ufrh_providers::{ProviderConfig, Wire, default_key_env, preset};
use ufrh_tools::PermissionMode;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProviderOverride {
    pub base_url: Option<String>,
    /// Env var holding the API key (default: OPENCODE_API_KEY / OPENROUTER_API_KEY).
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// model id → "chat_completions" | "responses" | "messages"
    #[serde(default)]
    pub wire_overrides: HashMap<String, Wire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub permission_mode: PermissionMode,
    #[serde(default)]
    pub providers: HashMap<String, ProviderOverride>,
    /// Compact the conversation when it nears this many tokens (0 = never). Default 100k.
    #[serde(default = "default_compact")]
    pub compact_at_tokens: u64,
    /// Run `bash` inside bubblewrap (read-only FS except the project dir) when installed.
    #[serde(default)]
    pub sandbox: bool,
    #[serde(default)]
    pub hooks: Vec<crate::HookConfig>,
    #[serde(default)]
    pub mcp: Vec<ufrh_mcp::McpServerConfig>,
}

fn default_compact() -> u64 {
    100_000
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "zen".into(),
            model: String::new(),
            permission_mode: PermissionMode::Ask,
            providers: HashMap::new(),
            compact_at_tokens: default_compact(),
            sandbox: false,
            hooks: vec![],
            mcp: vec![],
        }
    }
}

pub fn config_path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ufrh").join("config.toml")
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&path, toml::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Resolve a provider by name: preset (zen/go/openrouter) + user overrides + key from env.
    /// Any other name needs `base_url` and is treated as an OpenAI-compatible endpoint.
    pub fn provider_config(&self, name: &str) -> Result<ProviderConfig> {
        let ov = self.providers.get(name).cloned().unwrap_or_default();
        let mut cfg = match preset(name) {
            Some(c) => c,
            None => {
                let Some(base) = &ov.base_url else {
                    bail!("unknown provider `{name}`: set providers.{name}.base_url in the config");
                };
                ProviderConfig {
                    name: name.into(),
                    base_url: base.clone(),
                    api_key: None,
                    extra_headers: vec![],
                    wire_overrides: HashMap::new(),
                    default_wire: |_| Wire::ChatCompletions,
                }
            }
        };
        if let Some(b) = ov.base_url {
            cfg.base_url = b;
        }
        cfg.extra_headers.extend(ov.headers);
        cfg.wire_overrides.extend(ov.wire_overrides);
        let env = ov
            .api_key_env
            .unwrap_or_else(|| default_key_env(name).to_owned());
        cfg.api_key = std::env::var(&env).ok().filter(|k| !k.is_empty());
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_and_custom_provider() {
        let c: Config = toml::from_str(
            r#"
            provider = "local"
            model = "m"
            [providers.local]
            base_url = "http://localhost:8080/v1"
            [providers.zen]
            base_url = "http://proxy:1/v1"
            wire_overrides = { "foo" = "messages" }
            "#,
        )
        .unwrap();
        assert_eq!(
            c.provider_config("local").unwrap().base_url,
            "http://localhost:8080/v1"
        );
        let z = c.provider_config("zen").unwrap();
        assert_eq!(z.base_url, "http://proxy:1/v1");
        assert_eq!(z.wire_for("foo"), Wire::Messages);
        assert!(Config::default().provider_config("nope").is_err());
    }
}
