use crate::Tool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    /// Read-only tools run; everything else asks.
    #[default]
    Ask,
    /// Read-only and file edits run; `bash` and others ask.
    AcceptEdits,
    /// Read-only tools only (network tools still ask); everything else is denied.
    Plan,
    /// Like Claude Code's auto mode: routine project work runs, risky actions ask, and the rest
    /// is checked by a reviewer model (see `auto.rs` and `magia_core::review`).
    Auto,
    /// Everything runs without asking.
    Yolo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
    /// `auto` mode: classify the call, then the reviewer model if still unclear.
    Review,
}

pub fn decide(mode: PermissionMode, tool: &dyn Tool) -> Decision {
    if tool.network() {
        return match mode {
            PermissionMode::Yolo => Decision::Allow,
            PermissionMode::Auto => Decision::Review,
            _ => Decision::Ask,
        };
    }
    if tool.read_only() {
        return Decision::Allow;
    }
    match mode {
        PermissionMode::Yolo => Decision::Allow,
        PermissionMode::Plan => Decision::Deny,
        PermissionMode::AcceptEdits if tool.is_edit() => Decision::Allow,
        PermissionMode::Auto => Decision::Review,
        _ => Decision::Ask,
    }
}

// ───────────── rule sets (OpenCode-style `permission`) ─────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    Allow,
    Ask,
    Deny,
}

impl Effect {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "allow" => Some(Self::Allow),
            "ask" => Some(Self::Ask),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

/// Ordered rules; the **last** matching rule wins. `tool` is a tool name or `*`; `pattern` is
/// a glob matched against the call's key (bash command, file path, …).
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    /// (tool, original pattern, matcher, effect)
    rules: Vec<(String, String, globset::GlobMatcher, Effect)>,
}

fn matches_everything(pattern: &str) -> bool {
    matches!(pattern, "*" | "**")
}

impl RuleSet {
    pub fn push(
        &mut self,
        tool: &str,
        pattern: &str,
        effect: Effect,
    ) -> Result<(), globset::Error> {
        self.rules.push((
            tool.to_owned(),
            pattern.to_owned(),
            globset::Glob::new(pattern)?.compile_matcher(),
            effect,
        ));
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn check(&self, tool: &str, key: &str) -> Option<Effect> {
        self.rules
            .iter()
            .rev()
            .find(|(t, _, m, _)| (t == "*" || t == tool) && m.is_match(key))
            .map(|r| r.3)
    }

    /// True when no call to `tool` can ever run: some catch-all rule (`*`) denies it and no
    /// later rule for it says anything else. Such tools are not offered to the model.
    pub fn fully_denied(&self, tool: &str) -> bool {
        let applies = |t: &String| t == "*" || t == tool;
        let Some(i) = self
            .rules
            .iter()
            .rposition(|(t, p, _, e)| applies(t) && matches_everything(p) && *e == Effect::Deny)
        else {
            return false;
        };
        !self.rules[i + 1..]
            .iter()
            .any(|(t, _, _, e)| applies(t) && *e != Effect::Deny)
    }

    /// True when at least one call to `tool` could be allowed by a rule.
    pub fn may_allow(&self, tool: &str) -> bool {
        self.rules
            .iter()
            .any(|(t, _, _, e)| (t == "*" || t == tool) && *e == Effect::Allow)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_match_wins() {
        let mut r = RuleSet::default();
        r.push("bash", "*", Effect::Ask).unwrap();
        r.push("bash", "git *", Effect::Allow).unwrap();
        r.push("bash", "git push *", Effect::Deny).unwrap();
        assert_eq!(r.check("bash", "ls"), Some(Effect::Ask));
        assert_eq!(r.check("bash", "git status"), Some(Effect::Allow));
        assert_eq!(r.check("bash", "git push origin main"), Some(Effect::Deny));
        assert_eq!(r.check("read", "x"), None);
        assert!(!r.fully_denied("bash"));
    }

    #[test]
    fn fully_denied_needs_a_catch_all_deny_with_nothing_after() {
        let mut r = RuleSet::default();
        r.push("bash", "*", Effect::Deny).unwrap();
        assert!(r.fully_denied("bash"));
        r.push("bash", "echo *", Effect::Allow).unwrap();
        assert!(!r.fully_denied("bash"), "echo is still possible");
        let mut all = RuleSet::default();
        all.push("*", "*", Effect::Deny).unwrap();
        assert!(all.fully_denied("read"));
        let mut partial = RuleSet::default();
        partial.push("bash", "rm *", Effect::Deny).unwrap();
        assert!(!partial.fully_denied("bash"));
    }
}
