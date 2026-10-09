//! Offline, user-owned companion skill contract. Add environments here before
//! using their destination mapping; never auto-enroll a destination.
use clap::{Args, ValueEnum};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const CONTENT: &str = include_str!("../../../skills/fin/SKILL.md");
pub const VERSION: &str = "1.2.0";
pub const SETUP_PROMPT: &str = "Install Fin's optional skill";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AgentEnvironment {
    Codex,
    Claude,
}
impl AgentEnvironment {
    pub const ALL: &[Self] = &[Self::Codex, Self::Claude];
    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }
    pub fn directory(self) -> &'static str {
        match self {
            Self::Codex => ".agents/skills/fin",
            Self::Claude => ".claude/skills/fin",
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
        "name":"fin", "purpose":"Teach agents to prefer one bounded named plan for coherent multi-action financial intents, preserve separate approvals for standalone writes, respect human enrollment and plan approval, explicitly submit each step, and reconcile uncertain outcomes.",
        "version":VERSION, "bundled":true, "optional":true,
        "supported_agent_environments":AgentEnvironment::ALL.iter().map(|agent| json!({"name":agent.name(),"destination":format!("$HOME/{}",agent.directory())})).collect::<Vec<_>>(),
        "requires_administrator":false, "requires_financial_approval":false, "network_required":false, "service_required":false,
        "installation":{"command":"fin skill install --agent codex","guidance":"Run as your own user. Uses the embedded skill; no checkout or network required. Restart the agent if the skill does not appear."},
        "inspection":{"command":"fin skill inspect --agent codex --json"},
        "update":{"command":"fin skill update --agent codex","guidance":"Uses the skill bundled with this Fin binary. Fin runtime upgrades refresh only previously installed, unmodified Fin-managed copies; no new environments are installed."},
        "removal":{"command":"fin skill remove --agent codex"},
        "conflicts":{"default":"Preserve content and report SKILL_CONFLICT.","resolution":"Inspect the destination and move it aside yourself, or repeat install/update/remove with --backup to preserve conflicting content before replacement/removal. Symlinks and unsafe paths must be resolved manually."}
    })
}

pub fn definitions() -> Vec<fin_domain::Definition> {
    use fin_domain::{ExecutionActor, OperationKind};
    ["install", "inspect", "update", "remove"].into_iter().map(|action| {
        let inspect = action == "inspect";
        let mut definition = super::definition::<SkillChange>(
            &format!("skill.{action}"),
            match action { "install" => "Optionally install the bundled companion skill for the selected agent.", "inspect" => "Inspect the companion skill's destination, version and managed-content status.", "update" => "Refresh an existing Fin-managed skill from this binary's bundle.", _ => "Remove a Fin-managed skill, preserving conflicting content by default." },
            if inspect {OperationKind::Read} else {OperationKind::Configuration},
            if inspect {ExecutionActor::AgentReadable} else {ExecutionActor::AgentInvocable},
            if inspect {json!({"agent":"codex"})} else {json!({"agent":"codex","backup":false})},
            json!({"service_required":false,"requires_administrator":false,"requires_human_terminal":false,"financial_movement":false,"financial_authorization":false,"secret_input":"none","companion_skill":metadata(),"command_line_examples":AgentEnvironment::ALL.iter().map(|agent| format!("fin skill {action} --agent {} --json",agent.name())).collect::<Vec<_>>()}),
        );
        if inspect { definition.input_schema = super::schema::<SkillTarget>(); }
        definition.output_schema = super::schema::<SkillResult>();
        definition.completion = "Reports only local companion-skill files; grants no financial authority.".into();
        definition
    }).collect()
}
