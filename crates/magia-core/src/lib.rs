//! Agent loop, configuration, hooks, skills and sessions. Must not depend on any UI crate.

mod agent;
pub mod agents;
pub mod btw;
mod config;
mod ext;
mod hooks;
mod jsonc;
pub mod magia_mode;
pub mod media;
pub mod review;

pub use agent::{
    Agent, AgentEvent, Approver, AutoApprove, FolderAnswer, MagiaPhase, REVIEW_PARENT, UserInput,
    build_system_prompt,
};
pub use config::{Config, ProviderOverride, config_dir, config_path};
pub use ext::{
    Skill, SlashEntry, SlashKind, discover_skills, expand_command, expand_mentions, filter_files,
    filter_slash, frontmatter, project_files, slash_catalog,
};
pub use hooks::{HookConfig, HookOutcome, Hooks};
pub use magia_types::Image;
