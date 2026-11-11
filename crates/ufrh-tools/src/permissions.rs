use crate::Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Read-only tools run; everything else asks.
    #[default]
    Ask,
    /// Read-only and file edits run; `bash` and others ask.
    AcceptEdits,
    /// Read-only tools only; everything else is denied.
    Plan,
    /// Everything runs without asking.
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

pub fn decide(mode: PermissionMode, tool: &dyn Tool) -> Decision {
    if tool.read_only() {
        return Decision::Allow;
    }
    match mode {
        PermissionMode::Yolo => Decision::Allow,
        PermissionMode::Plan => Decision::Deny,
        PermissionMode::AcceptEdits if tool.is_edit() => Decision::Allow,
        _ => Decision::Ask,
    }
}
