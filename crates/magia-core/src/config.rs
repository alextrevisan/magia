//! Configuration: JSON/JSONC, a superset of OpenCode's `opencode.json`.
//!
//! Layers (later wins, objects merge key by key): `~/.config/magia/magia.json[c]`, then
//! `<project>/magia.json[c]`, then `<project>/.magia/magia.json[c]`, then `$MAGIA_CONFIG`.
//! OpenCode keys understood: `model` (`provider/model`), `provider`/`providers`
//! (`options.baseURL`, `options.apiKey`, `options.headers`, `npm`, `env`, `models`),
//! `mcp` (local servers), `permission`/`permissions`, `instructions`, `command`/`commands`,
//! with `{env:VAR}` and `{file:path}` substitution. magia-only settings live under `"magia"`.
//! The user's config is never rewritten; the last GUI selection goes to a separate state file.

use crate::HookConfig;
use crate::jsonc::{deep_merge, strip_jsonc, substitute};
use anyhow::{Context, Result};
use magia_mcp::McpServerConfig;
use magia_providers::{ProviderConfig, Wire, default_key_env, preset};
use magia_tools::{Effect, PermissionMode, RuleSet};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct ProviderOverride {
    pub base_url: Option<String>,
    /// Literal key (normally written as `{env:NAME}` in the file, so it is already resolved).
    pub api_key: Option<String>,
    /// Name of the env var holding the key.
    pub api_key_env: Option<String>,
    /// OpenCode's `env: ["VAR", …]`: candidate variable names.
    pub env: Vec<String>,
    pub headers: HashMap<String, String>,
    pub wire_overrides: HashMap<String, Wire>,
    /// Wire for models without an override (from OpenCode's `npm` package name).
    pub default_wire: Option<Wire>,
    /// Model ids declared in the config (added to the model picker).
    pub models: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub provider: String,
    pub model: String,
    pub permission_mode: PermissionMode,
    /// Primary agent: `build`, `plan`, `magia` or a root `.md` file name (`crate::agents`).
    /// From OpenCode's `default_agent`, then the last GUI choice.
    pub agent: String,
    pub providers: HashMap<String, ProviderOverride>,
    /// Compact the conversation when it nears this many tokens (0 = never). Default 100k.
    pub compact_at_tokens: u64,
    /// Run `bash` inside bubblewrap (read-only FS except the project dir) when installed.
    pub sandbox: bool,
    /// `auto` mode: model id that reviews unclear tool calls (default: the session's model).
    pub auto_model: Option<String>,
    /// Limits of the Magia agent (`"magia": {"magia_mode": {...}}`).
    pub magia_mode: crate::magia_mode::MagiaMode,
    pub hooks: Vec<HookConfig>,
    pub mcp: Vec<McpServerConfig>,
    pub rules: RuleSet,
    /// Extra instruction files/globs (relative to the project) added to the system prompt.
    pub instructions: Vec<String>,
    /// Slash commands defined in the config: name → template.
    pub commands: HashMap<String, String>,
    /// `description` of each config command (for the `/` autocomplete).
    pub command_descriptions: HashMap<String, String>,
    /// Things in the config that are understood but not supported (shown to the user).
    pub warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "zen".into(),
            model: String::new(),
            permission_mode: PermissionMode::Ask,
            agent: crate::agents::BUILD.into(),
            providers: HashMap::new(),
            compact_at_tokens: 100_000,
            sandbox: false,
            auto_model: None,
            magia_mode: Default::default(),
            hooks: vec![],
            mcp: vec![],
            rules: RuleSet::default(),
            instructions: vec![],
            commands: HashMap::new(),
            command_descriptions: HashMap::new(),
            warnings: vec![],
        }
    }
}

/// `$HOME`, or `%USERPROFILE%` on Windows.
pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

/// `$XDG_CONFIG_HOME/magia` (or `~/.config/magia`).
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("magia")
}

/// Global config file (`magia.jsonc` if it exists, else `magia.json`).
pub fn config_path() -> PathBuf {
    first_existing(&config_dir(), "magia").unwrap_or_else(|| config_dir().join("magia.json"))
}

fn state_path() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"))
        .join("magia")
        .join("state.json")
}

fn first_existing(dir: &Path, stem: &str) -> Option<PathBuf> {
    ["jsonc", "json"]
        .iter()
        .map(|e| dir.join(format!("{stem}.{e}")))
        .find(|p| p.is_file())
}

/// Read one layer: strip JSONC, parse, resolve `{env:}`/`{file:}`.
fn read_layer(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut v: Value = serde_json::from_str(&strip_jsonc(&text))
        .with_context(|| format!("parsing {}", path.display()))?;
    substitute(&mut v, path.parent().unwrap_or(Path::new(".")));
    Ok(v)
}

impl Config {
    pub fn load() -> Result<Self> {
        Self::load_from(&std::env::current_dir()?)
    }

    pub fn load_from(cwd: &Path) -> Result<Self> {
        let mut layers: Vec<PathBuf> = Vec::new();
        layers.extend(first_existing(&config_dir(), "magia"));
        layers.extend(first_existing(cwd, "magia"));
        layers.extend(first_existing(&cwd.join(".magia"), "magia"));
        if let Some(p) = std::env::var_os("MAGIA_CONFIG").filter(|p| !p.is_empty()) {
            layers.push(PathBuf::from(p));
        }
        let mut merged = Value::Object(Map::new());
        for p in &layers {
            deep_merge(&mut merged, read_layer(p)?);
        }
        let mut cfg = Self::from_value(&merged);
        cfg.apply_state();
        Ok(cfg)
    }

    /// Build a config from an already merged/substituted JSON document.
    pub fn from_value(v: &Value) -> Self {
        let mut cfg = Self::default();
        let obj = |x: &Value| x.as_object().cloned().unwrap_or_default();
        let str_of = |x: Option<&Value>| {
            x.and_then(Value::as_str)
                .map(str::to_owned)
                .filter(|s| !s.is_empty())
        };

        // model: "provider/model[#variant]" or {providerID, model}
        match v.get("model") {
            Some(Value::String(m)) => cfg.set_model(m),
            Some(Value::Object(o)) => {
                if let (Some(p), Some(m)) = (str_of(o.get("providerID")), str_of(o.get("model"))) {
                    cfg.provider = p;
                    cfg.model = m;
                }
            }
            _ => {}
        }

        if let Some(a) = str_of(v.get("default_agent")) {
            cfg.agent = a;
        }

        // providers (new key `providers`, legacy `provider`)
        for key in ["provider", "providers"] {
            for (id, p) in obj(v.get(key).unwrap_or(&Value::Null)) {
                let ov = cfg.providers.entry(id).or_default();
                for block in ["options", "settings"] {
                    let o = obj(p.get(block).unwrap_or(&Value::Null));
                    ov.base_url = ov.base_url.take().or(["baseURL", "baseUrl", "base_url"]
                        .iter()
                        .find_map(|k| str_of(o.get(*k))));
                    ov.api_key = ov
                        .api_key
                        .take()
                        .or(["apiKey", "api_key"].iter().find_map(|k| str_of(o.get(*k))));
                    for (hk, hv) in obj(o.get("headers").unwrap_or(&Value::Null)) {
                        if let Some(s) = hv.as_str() {
                            ov.headers.insert(hk, s.to_owned());
                        }
                    }
                }
                ov.base_url = ov.base_url.take().or(str_of(p.get("api")));
                if let Some(a) = p.get("env").and_then(Value::as_array) {
                    ov.env = a
                        .iter()
                        .filter_map(|e| e.as_str().map(str::to_owned))
                        .collect();
                }
                ov.default_wire = match str_of(p.get("npm")).as_deref() {
                    Some("@ai-sdk/anthropic") => Some(Wire::Messages),
                    Some("@ai-sdk/openai") => Some(Wire::Responses),
                    Some("@ai-sdk/openai-compatible") => Some(Wire::ChatCompletions),
                    _ => ov.default_wire,
                };
                ov.models
                    .extend(obj(p.get("models").unwrap_or(&Value::Null)).keys().cloned());
            }
        }

        // MCP: new `mcp.servers.<name>`, legacy `mcp.<name>`
        let mcp = obj(v.get("mcp").unwrap_or(&Value::Null));
        let mut servers: Vec<(String, Value)> = obj(mcp.get("servers").unwrap_or(&Value::Null))
            .into_iter()
            .collect();
        servers.extend(
            mcp.iter()
                .filter(|(k, _)| !["servers", "timeout"].contains(&k.as_str()))
                .map(|(k, s)| (k.clone(), s.clone())),
        );
        for (name, s) in servers {
            let off = s.get("disabled").and_then(Value::as_bool) == Some(true)
                || s.get("enabled").and_then(Value::as_bool) == Some(false);
            if off {
                continue;
            }
            match s.get("type").and_then(Value::as_str) {
                Some("local") => {
                    let cmd: Vec<String> = s
                        .get("command")
                        .and_then(Value::as_array)
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default();
                    let Some((program, args)) = cmd.split_first() else {
                        cfg.warnings.push(format!("mcp `{name}`: empty command"));
                        continue;
                    };
                    let env = obj(s.get("environment").unwrap_or(&Value::Null))
                        .into_iter()
                        .filter_map(|(k, v)| Some((k, v.as_str()?.to_owned())))
                        .collect();
                    cfg.mcp.push(McpServerConfig {
                        name,
                        command: program.clone(),
                        args: args.to_vec(),
                        env,
                    });
                }
                Some("remote") => cfg.warnings.push(format!(
                    "mcp `{name}`: remote (HTTP) servers are not supported yet"
                )),
                _ => {}
            }
        }

        // permissions: legacy object / string, or new ordered array
        match v.get("permissions").or_else(|| v.get("permission")) {
            Some(Value::String(e)) => cfg.add_rule("*", "*", e),
            Some(Value::Object(o)) => {
                for (tool, rule) in o {
                    for name in magia_tool_names(tool) {
                        match rule {
                            Value::String(e) => cfg.add_rule(&name, "*", e),
                            Value::Object(patterns) => {
                                for (pat, e) in patterns {
                                    cfg.add_rule(&name, pat, e.as_str().unwrap_or(""));
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            Some(Value::Array(rules)) => {
                for r in rules {
                    let action = r.get("action").and_then(Value::as_str).unwrap_or("*");
                    let (resource, effect) = (
                        r.get("resource").and_then(Value::as_str).unwrap_or("*"),
                        r.get("effect").and_then(Value::as_str).unwrap_or(""),
                    );
                    for name in magia_tool_names(action) {
                        cfg.add_rule(&name, resource, effect);
                    }
                }
            }
            _ => {}
        }

        if let Some(a) = v.get("instructions").and_then(Value::as_array) {
            cfg.instructions = a
                .iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect();
        }
        for key in ["command", "commands"] {
            for (name, c) in obj(v.get(key).unwrap_or(&Value::Null)) {
                if let Some(t) = c.get("template").and_then(Value::as_str) {
                    if let Some(d) = c.get("description").and_then(Value::as_str) {
                        cfg.command_descriptions.insert(name.clone(), d.to_owned());
                    }
                    cfg.commands.insert(name, t.to_owned());
                }
            }
        }

        // magia-only settings
        let u = obj(v.get("magia").unwrap_or(&Value::Null));
        if let Some(m) = u
            .get("permission_mode")
            .and_then(|m| serde_json::from_value(m.clone()).ok())
        {
            cfg.permission_mode = m;
        }
        if let Some(n) = u.get("compact_at_tokens").and_then(Value::as_u64) {
            cfg.compact_at_tokens = n;
        }
        cfg.sandbox = u.get("sandbox").and_then(Value::as_bool).unwrap_or(false);
        if let Some(m) = u
            .get("auto_model")
            .and_then(Value::as_str)
            .filter(|m| !m.is_empty())
        {
            cfg.auto_model = Some(m.to_owned());
        }
        if let Some(m) = u
            .get("magia_mode")
            .and_then(|m| serde_json::from_value(m.clone()).ok())
        {
            cfg.magia_mode = m;
        }
        if let Some(h) = u
            .get("hooks")
            .and_then(|h| serde_json::from_value(h.clone()).ok())
        {
            cfg.hooks = h;
        }
        // { "provider/model": "messages" | "responses" | "chat_completions" }
        for (pm, w) in obj(u.get("wire_overrides").unwrap_or(&Value::Null)) {
            if let (Some((prov, model)), Ok(w)) =
                (pm.split_once('/'), serde_json::from_value::<Wire>(w))
            {
                cfg.providers
                    .entry(prov.to_owned())
                    .or_default()
                    .wire_overrides
                    .insert(model.to_owned(), w);
            }
        }
        cfg
    }

    fn set_model(&mut self, m: &str) {
        let m = m.split('#').next().unwrap_or(m);
        match m.split_once('/') {
            Some((p, rest)) => {
                self.provider = p.to_owned();
                self.model = rest.to_owned();
            }
            None => self.model = m.to_owned(),
        }
    }

    fn add_rule(&mut self, tool: &str, pattern: &str, effect: &str) {
        match Effect::parse(effect) {
            Some(e) => {
                if self.rules.push(tool, pattern, e).is_err() {
                    self.warnings
                        .push(format!("permission: invalid pattern `{pattern}`"));
                }
            }
            None => self
                .warnings
                .push(format!("permission `{tool}`: unknown effect `{effect}`")),
        }
    }

    /// Last GUI selection (provider/model/mode), kept outside the user's config file.
    fn apply_state(&mut self) {
        let Ok(text) = std::fs::read_to_string(state_path()) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        if let Some(p) = v.get("provider").and_then(Value::as_str) {
            self.provider = p.to_owned();
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = m.to_owned();
        }
        if let Some(m) = v
            .get("permission_mode")
            .and_then(|m| serde_json::from_value(m.clone()).ok())
        {
            self.permission_mode = m;
        }
        if let Some(a) = v.get("agent").and_then(Value::as_str) {
            self.agent = a.to_owned();
        }
    }

    pub fn save_state(&self) -> Result<()> {
        let path = state_path();
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let v = serde_json::json!({
            "provider": self.provider,
            "model": self.model,
            "permission_mode": self.permission_mode,
            "agent": self.agent,
        });
        std::fs::write(path, serde_json::to_string_pretty(&v)?)?;
        Ok(())
    }

    /// Models declared for a provider in the config.
    pub fn declared_models(&self, provider: &str) -> Vec<String> {
        self.providers
            .get(provider)
            .map(|p| p.models.clone())
            .unwrap_or_default()
    }

    /// Resolve a provider by name: preset (zen/go/openrouter) + user overrides + key from env.
    /// Any other name needs a base URL and is treated as an OpenAI-compatible endpoint.
    pub fn provider_config(&self, name: &str) -> Result<ProviderConfig> {
        let ov = self.providers.get(name).cloned().unwrap_or_default();
        let mut cfg = match preset(name) {
            Some(c) => c,
            None => {
                let Some(base) = &ov.base_url else {
                    anyhow::bail!(
                        "unknown provider `{name}`: define it under \"provider\" with options.baseURL in the config"
                    );
                };
                ProviderConfig {
                    name: name.into(),
                    base_url: base.clone(),
                    api_key: None,
                    extra_headers: vec![],
                    wire_overrides: HashMap::new(),
                    default_wire: |_| Wire::ChatCompletions,
                    session_header: None,
                }
            }
        };
        if let Some(b) = ov.base_url {
            cfg.base_url = b;
        }
        if let Some(w) = ov.default_wire {
            cfg.default_wire = match w {
                Wire::ChatCompletions => |_| Wire::ChatCompletions,
                Wire::Responses => |_| Wire::Responses,
                Wire::Messages => |_| Wire::Messages,
            };
        }
        cfg.extra_headers.extend(ov.headers);
        cfg.wire_overrides.extend(ov.wire_overrides);
        let from_env = |n: &String| std::env::var(n).ok().filter(|k| !k.is_empty());
        cfg.api_key = ov
            .api_key
            .or_else(|| ov.api_key_env.as_ref().and_then(from_env))
            .or_else(|| ov.env.iter().find_map(from_env))
            .or_else(|| from_env(&default_key_env(name).to_owned()));
        Ok(cfg)
    }
}

/// OpenCode permission/tool names → magia tool names.
fn magia_tool_names(name: &str) -> Vec<String> {
    let v: &[&str] = match name {
        "edit" => &["edit", "write"],
        "shell" => &["bash"],
        "list" => &["ls"],
        "todowrite" => &["todo"],
        "*" => &["*"],
        other => return vec![other.to_owned()],
    };
    v.iter().map(|s| (*s).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn opencode_style_config_is_understood() {
        let v = json!({
            "$schema": "https://opencode.ai/config.json",
            "model": "openrouter/anthropic/claude-sonnet-4#high",
            "provider": {
                "local": {"npm": "@ai-sdk/openai-compatible", "options": {"baseURL": "http://localhost:8080/v1", "apiKey": "sk-local", "headers": {"X-A": "1"}},
                          "models": {"m1": {}, "m2": {}}},
                "zen": {"options": {"baseURL": "http://proxy/v1"}},
                "anthro": {"npm": "@ai-sdk/anthropic", "options": {"baseURL": "https://x/v1"}}
            },
            "mcp": {"fs": {"type": "local", "command": ["npx", "-y", "srv", "."], "environment": {"A": "b"}},
                    "off": {"type": "local", "command": ["x"], "enabled": false},
                    "web": {"type": "remote", "url": "https://m"}},
            "permission": {"bash": {"*": "ask", "git status*": "allow"}, "edit": "allow", "list": "deny"},
            "instructions": ["CONTRIBUTING.md"],
            "command": {"review": {"template": "Review $ARGUMENTS"}},
            "magia": {"permission_mode": "accept_edits", "compact_at_tokens": 5, "sandbox": true,
                     "magia_mode": {"max_rounds": 7},
                     "wire_overrides": {"zen/foo": "messages"}}
        });
        let c = Config::from_value(&v);
        assert_eq!(
            (c.provider.as_str(), c.model.as_str()),
            ("openrouter", "anthropic/claude-sonnet-4")
        );
        let l = c.provider_config("local").unwrap();
        assert_eq!(l.base_url, "http://localhost:8080/v1");
        assert_eq!(l.api_key.as_deref(), Some("sk-local"));
        assert_eq!(l.extra_headers, vec![("X-A".to_owned(), "1".to_owned())]);
        assert_eq!(c.declared_models("local"), vec!["m1", "m2"]);
        assert_eq!(
            c.provider_config("zen").unwrap().base_url,
            "http://proxy/v1"
        );
        assert_eq!(
            c.provider_config("zen").unwrap().wire_for("foo"),
            Wire::Messages
        );
        assert_eq!(
            c.provider_config("anthro").unwrap().wire_for("any"),
            Wire::Messages
        );
        assert_eq!(c.mcp.len(), 1);
        assert_eq!((c.mcp[0].command.as_str(), c.mcp[0].args.len()), ("npx", 3));
        assert!(c.warnings.iter().any(|w| w.contains("remote")));
        assert_eq!(c.rules.check("bash", "git status -s"), Some(Effect::Allow));
        assert_eq!(c.rules.check("bash", "rm x"), Some(Effect::Ask));
        assert_eq!(c.rules.check("write", "a.rs"), Some(Effect::Allow));
        assert_eq!(c.rules.check("ls", "."), Some(Effect::Deny));
        assert_eq!(c.instructions, vec!["CONTRIBUTING.md"]);
        assert_eq!(c.commands["review"], "Review $ARGUMENTS");
        assert_eq!(
            (c.permission_mode, c.compact_at_tokens, c.sandbox),
            (PermissionMode::AcceptEdits, 5, true)
        );
        // Keys left out keep their defaults.
        assert_eq!(
            c.magia_mode,
            crate::magia_mode::MagiaMode {
                max_rounds: 7,
                review: true
            }
        );
        assert!(Config::default().provider_config("nope").is_err());
    }

    #[test]
    fn new_style_permissions_array_and_providers_key() {
        let c = Config::from_value(&json!({
            "providers": {"p": {"settings": {"apiKey": "k"}, "api": "http://a/v1"}},
            "permissions": [{"action": "shell", "resource": "git push *", "effect": "ask"}, {"action": "edit", "resource": "*", "effect": "deny"}]
        }));
        assert_eq!(
            c.provider_config("p").unwrap().api_key.as_deref(),
            Some("k")
        );
        assert_eq!(c.provider_config("p").unwrap().base_url, "http://a/v1");
        assert_eq!(c.rules.check("bash", "git push origin"), Some(Effect::Ask));
        assert_eq!(c.rules.check("edit", "x"), Some(Effect::Deny));
    }

    #[test]
    fn layers_load_from_disk_with_comments_and_env() {
        let d = tempfile::tempdir().unwrap();
        // SAFETY: only this test touches these variables; isolate from the real user config/state.
        unsafe {
            std::env::set_var("MAGIA_TEST_KEY", "secret-from-env");
            std::env::set_var("XDG_CONFIG_HOME", d.path().join("xdg-config"));
            std::env::set_var("XDG_STATE_HOME", d.path().join("xdg-state"));
        }
        std::fs::write(
            d.path().join("magia.jsonc"),
            "{\n // project config\n \"model\": \"local/m1\",\n \"provider\": {\"local\": {\"options\": {\"baseURL\": \"http://h/v1\", \"apiKey\": \"{env:MAGIA_TEST_KEY}\"}}},\n}\n",
        )
        .unwrap();
        let c = Config::load_from(d.path()).unwrap();
        assert_eq!(
            c.provider_config("local").unwrap().api_key.as_deref(),
            Some("secret-from-env")
        );
        assert_eq!(c.provider, "local");
        std::fs::write(d.path().join("magia.jsonc"), "{ \"model\": ").unwrap();
        let e = Config::load_from(d.path()).unwrap_err().to_string();
        assert!(e.contains("magia.jsonc"), "{e}");
    }

    #[test]
    fn documented_example_stays_valid() {
        let text = include_str!("../../../docs/magia.example.jsonc");
        let v: Value = serde_json::from_str(&strip_jsonc(text)).expect("example parses as JSONC");
        let c = Config::from_value(&v);
        assert_eq!(
            (c.provider.as_str(), c.model.as_str()),
            ("zen", "big-pickle")
        );
        assert_eq!(c.declared_models("local"), vec!["my-model"]);
        assert_eq!(c.mcp.len(), 1);
        assert_eq!(c.hooks.len(), 1);
        assert_eq!(c.rules.check("bash", "rm -rf x"), Some(Effect::Deny));
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    }
}
