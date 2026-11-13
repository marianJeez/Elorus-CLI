//! The kinds of subagent the `task` tool starts: one TOML file per kind in data/agents/,
//! registered in `SOURCES`. Each says what it is for, which of the parent's tools it gets, what
//! it adds to the parent's system prompt, and the mode its subagent runs in when that differs
//! from the parent's.

use std::sync::LazyLock;

use serde::{Deserialize, Deserializer};

use crate::config::limits;
use crate::data;
use crate::mode::{self, Mode};
use crate::tools::Registry;

const SOURCES: &[&str] = &[
    data::raw!("agents/explore.toml"),
    data::raw!("agents/general.toml"),
    data::raw!("agents/coder.toml"),
    data::raw!("agents/reviewer.toml"),
];

static KINDS: LazyLock<Vec<Kind>> = LazyLock::new(|| {
    SOURCES
        .iter()
        .map(|src| data::parse(src, "agents/*.toml"))
        .collect()
});

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Kind {
    pub id: String,
    pub summary: String,
    /// The parent's tools it gets, by name; `*` is all of them.
    tools: Vec<String>,
    /// Tools a `*` leaves out.
    #[serde(default)]
    except: Vec<String>,
    /// The mode this kind asks for, when it differs from its parent's; the parent's mode decides
    /// whether kinds may bring one (`subagent_modes`).
    #[serde(default, deserialize_with = "mode_by_id")]
    mode: Option<&'static Mode>,
    pub prompt: String,
}

/// A kind names a real mode, so a typo fails when the kinds load rather than silently leaving the
/// subagent in its parent's mode.
fn mode_by_id<'de, D: Deserializer<'de>>(d: D) -> Result<Option<&'static Mode>, D::Error> {
    Option::<String>::deserialize(d)?
        .map(|id| mode::find(&id).map_err(serde::de::Error::custom))
        .transpose()
}

impl Kind {
    /// The mode this kind asks for; `None` inherits the parent's.
    pub fn mode(&self) -> Option<&'static Mode> {
        self.mode
    }
}

pub fn all() -> &'static [Kind] {
    &KINDS
}

pub fn get(id: &str) -> Option<&'static Kind> {
    KINDS.iter().find(|k| k.id == id)
}

/// The tools a subagent of `kind` gets from its parent's `registry`, `depth` runs below the
/// frontend's: never `task` at the depth limit, so it is never offered a call it could not make.
pub fn tools(kind: &Kind, registry: &Registry, depth: usize) -> Registry {
    let deepest = depth >= limits::get().subagent.max_depth.value;
    let named: Vec<&str> = match kind.tools.iter().any(|t| t == "*") {
        true => registry.each_name().collect(),
        false => kind.tools.iter().map(String::as_str).collect(),
    };
    let names: Vec<&str> = named
        .into_iter()
        .filter(|name| !kind.except.iter().any(|e| e == name))
        .filter(|name| !(deepest && *name == crate::tools::TASK))
        .collect();
    registry.subset(&names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::catalog;
    use crate::tools::testing::{Project, sample};

    #[test]
    fn kinds_have_unique_ids_and_name_real_tools() {
        let registry = Registry::builtin(None);
        let mut ids: Vec<&str> = all().iter().map(|k| k.id.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), all().len());
        for kind in all() {
            for tool in kind.tools.iter().chain(&kind.except).filter(|t| *t != "*") {
                assert!(registry.get(tool).is_some(), "{}: no tool {tool}", kind.id);
            }
        }
    }

    #[test]
    fn general_gets_every_tool_but_handing_over_the_plan_and_starting_another() {
        let registry = Registry::builtin(None);
        let general = tools(get("general").unwrap(), &registry, 1);
        let want: Vec<&str> = registry
            .each_name()
            .filter(|n| ![crate::tools::PLAN_EXIT, crate::tools::TASK].contains(n))
            .collect();
        assert_eq!(general.each_name().collect::<Vec<_>>(), want);
    }

    #[test]
    fn explore_changes_nothing_and_never_gets_task() {
        let explore = get("explore").unwrap();
        let tools = tools(explore, &Registry::builtin(None), 1);
        assert!(tools.get(crate::tools::TASK).is_none());
        let project = Project::new();
        for name in &explore.tools {
            let tool = tools.get(name).unwrap();
            let args = sample(&tool.spec().parameters);
            if let Ok(check) = project.check(tool.as_ref(), &args) {
                for ask in check.asks {
                    assert!(!catalog::changes_things(&ask.permission), "{name}: {ask:?}");
                }
            }
        }
    }

    #[test]
    fn coder_builds_and_reviewer_reviews_without_changing_files() {
        let registry = Registry::builtin(None);
        let coder = get("coder").unwrap();
        assert_eq!(coder.mode().map(|m| m.id.as_str()), Some("build"));
        let coder_tools = tools(coder, &registry, 1);
        assert!(coder_tools.get("edit").is_some());
        assert!(coder_tools.get("write").is_some());
        assert!(coder_tools.get(crate::tools::TASK).is_none());
        assert!(coder_tools.get(crate::tools::PLAN_EXIT).is_none());

        let reviewer = get("reviewer").unwrap();
        assert_eq!(reviewer.mode().map(|m| m.id.as_str()), Some("plan"));
        let reviewer_tools = tools(reviewer, &registry, 1);
        assert!(reviewer_tools.get("read").is_some());
        assert!(reviewer_tools.get("bash").is_some());
        assert!(reviewer_tools.get("edit").is_none());
        assert!(reviewer_tools.get(crate::tools::TASK).is_none());
    }
}
