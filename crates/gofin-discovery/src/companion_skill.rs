//! Offline, user-owned companion skill contract. Add environments here before
//! using their destination mapping; never auto-enroll a destination.
use clap::{Args, ValueEnum};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CONTENT: &str = include_str!("../../../skills/gofin/SKILL.md");
pub const VERSION: &str = "1.9.0";
pub const SETUP_PROMPT: &str = "Install GoFin's optional skill";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentEnvironment {
    Codex,
    Claude,
    Hermes,
    Openclaw,
    // Grok Bot: prepare a local export for manual import into its private skills.
    Grok,
}
impl AgentEnvironment {
    pub const ALL: &[Self] = &[
        Self::Codex,
        Self::Claude,
        Self::Hermes,
        Self::Openclaw,
        Self::Grok,
    ];
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Hermes => "hermes",
            Self::Openclaw => "openclaw",
            Self::Grok => "grok",
        }
    }
    pub fn directory(self) -> &'static str {
        match self {
            Self::Codex => ".agents/skills/gofin",
            Self::Claude => ".claude/skills/gofin",
            Self::Hermes => ".hermes/skills/gofin",
            Self::Openclaw => ".openclaw/skills/gofin",
            Self::Grok => ".local/share/gofin/skill-exports/grok/gofin",
        }
    }
}

#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillTarget {
    /// Agent environment to inspect or manage; never selected automatically.
    #[arg(long, value_enum)]
    pub agent: AgentEnvironment,
}

#[derive(Args, Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillChange {
    #[command(flatten)]
    #[serde(flatten)]
    pub target: SkillTarget,
    /// Preserve conflicting content in a local backup before replacing/removing it.
    #[arg(long)]
    #[serde(default)]
    pub backup: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SkillStatus {
    NotInstalled,
    Unmanaged,
    Modified,
    Current,
    Outdated,
    Installed,
    Updated,
    Removed,
}

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SkillResult {
    pub agent: AgentEnvironment,
    /// Local managed-file status only; does not establish agent loading or Grok Bot import.
    pub status: SkillStatus,
    pub path: String,
    pub bundled_version: String,
    pub installed_version: Option<String>,
    pub managed: bool,
    pub backup_path: Option<String>,
    pub message: String,
    pub next_action: String,
}

pub fn metadata() -> Value {
    json!({
        "name":"gofin", "purpose":"Teach agents to discover installed GoFin capabilities, choose only advertised authorization and execution mechanisms, respect human enrollment and review, explicitly submit each action, and reconcile uncertain outcomes.",
        "version":VERSION, "bundled":true, "optional":true,
        "supported_agent_environments":AgentEnvironment::ALL.iter().map(|agent| {
            let mut target = json!({"name":agent.name(),"destination":format!("$HOME/{}",agent.directory())});
            if *agent == AgentEnvironment::Grok {
                target["installation_mode"] = json!("manual_import");
                target["guidance"] = json!("Grok Bot uses a private skill library. Attach the exported SKILL.md in a Bot chat and ask it to save or replace the private gofin skill, preserving the instructions, and run GoFin through local command execution on your Mac. Verify the saved skill under Connect apps > Your plugins > Manage plugins and skills > Private skills. GoFin manages only the local export: inspect cannot verify import, updates require re-import, and removal leaves any private copy in Grok Bot unchanged.");
            }
            target
        }).collect::<Vec<_>>(),
        "requires_administrator":false, "requires_financial_approval":false, "network_required":false, "service_required":false,
        "installation":{"command":"gofin skill install --agent codex","guidance":"Run as your own user. Uses the embedded skill; no checkout or network required to prepare local files. Manages default user-home skill directories, or a local export for Grok Bot's manual import; custom agent profile roots and environment overrides are not resolved. Follow the selected target's next action."},
        "execution":{"guidance":"The agent's execution environment needs gofin on PATH and access to the local broker for financial work. Skill installation does not verify agent discovery, broker connectivity or financial execution; sandbox and remote execution require separate configuration and verification."},
        "inspection":{"command":"gofin skill inspect --agent codex --json"},
        "update":{"command":"gofin skill update --agent codex","guidance":"Uses the skill bundled with this GoFin binary. GoFin runtime upgrades refresh only previously installed, unmodified GoFin-managed local copies; no new environments are installed. Grok Bot exports must be re-imported to update the private skill."},
        "removal":{"command":"gofin skill remove --agent codex"},
        "conflicts":{"default":"Preserve content and report SKILL_CONFLICT.","resolution":"Inspect the destination and move it aside yourself, or repeat install/update/remove with --backup to preserve conflicting content before replacement/removal. Symlinks and unsafe paths must be resolved manually."}
    })
}

pub fn definitions() -> Vec<gofin_domain::Definition> {
    use gofin_domain::{ExecutionActor, OperationKind};
    ["install", "inspect", "update", "remove"].into_iter().map(|action| {
        let inspect = action == "inspect";
        let mut definition = super::definition::<SkillChange>(
            &format!("skill.{action}"),
            match action { "install" => "Optionally install the bundled companion skill for the selected agent.", "inspect" => "Inspect the companion skill's destination, version and managed-content status.", "update" => "Refresh an existing GoFin-managed skill from this binary's bundle.", _ => "Remove a GoFin-managed skill, preserving conflicting content by default." },
            if inspect {OperationKind::Read} else {OperationKind::Configuration},
            if inspect {ExecutionActor::AgentReadable} else {ExecutionActor::AgentInvocable},
            if inspect {json!({"agent":"codex"})} else {json!({"agent":"codex","backup":false})},
            json!({"service_required":false,"requires_administrator":false,"requires_human_terminal":false,"financial_movement":false,"financial_authorization":false,"secret_input":"none","companion_skill":metadata(),"command_line_examples":AgentEnvironment::ALL.iter().map(|agent| format!("gofin skill {action} --agent {} --json",agent.name())).collect::<Vec<_>>()}),
        );
        if inspect { definition.input_schema = super::schema::<SkillTarget>(); }
        definition.output_schema = super::schema::<SkillResult>();
        definition.completion = "Reports only local companion-skill files; grants no financial authority.".into();
        definition
    }).collect()
}
