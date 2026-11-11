//! Agent loop, configuration, hooks, skills and sessions. Must not depend on any UI crate.

mod agent;
mod config;
mod ext;
mod hooks;

pub use agent::{Agent, AgentEvent, Approver, AutoApprove, build_system_prompt};
pub use config::{Config, ProviderOverride, config_path};
pub use ext::{Skill, discover_skills, expand_command, expand_mentions, frontmatter};
pub use hooks::{HookConfig, HookOutcome, Hooks};
