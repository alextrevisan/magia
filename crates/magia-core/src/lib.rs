//! Agent loop, configuration, hooks, skills and sessions. Must not depend on any UI crate.

mod agent;
mod config;
mod ext;
mod hooks;
mod jsonc;
pub mod media;

pub use agent::{Agent, AgentEvent, Approver, AutoApprove, UserInput, build_system_prompt};
pub use config::{Config, ProviderOverride, config_dir, config_path};
pub use ext::{
    Skill, SlashEntry, SlashKind, discover_skills, expand_command, expand_mentions, filter_slash,
    frontmatter, slash_catalog,
};
pub use hooks::{HookConfig, HookOutcome, Hooks};
pub use magia_types::Image;
