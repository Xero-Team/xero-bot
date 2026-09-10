use std::collections::{BTreeMap, HashSet};

use serde::Deserialize;
use serde_json::Value;

pub const CONFIG_PATH: &str = ".github/xero-bot.toml";

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryConfig {
    idle_workflows: Option<Rules>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rules {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_idle")]
    pub idle_minutes: u32,
    #[serde(default)]
    pub monitors: Vec<Monitor>,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Monitor {
    /// Omitted means the repository containing the configuration.
    pub repository: Option<String>,
    /// Empty means all Actions workflows, regardless of their trigger event.
    #[serde(default)]
    pub workflows: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub workflow: String,
    pub branch: String,
    #[serde(default)]
    pub inputs: BTreeMap<String, Value>,
    #[serde(default = "default_retry_interval")]
    pub retry_interval_minutes: u32,
    #[serde(default = "default_retries")]
    pub max_retries: u32,
    /// Only these events constitute equivalent executions of the task. In
    /// particular, a nightly schedule may build a different SHA than run.head_sha.
    #[serde(default = "default_run_events")]
    pub run_events: Vec<String>,
}

fn default_idle() -> u32 {
    30
}
fn default_retry_interval() -> u32 {
    15
}
fn default_retries() -> u32 {
    2
}
fn default_run_events() -> Vec<String> {
    vec!["workflow_dispatch".into()]
}

pub fn workflow_name(value: &str) -> Result<&str, String> {
    let name = value.strip_prefix(".github/workflows/").unwrap_or(value);
    if name.is_empty() || name.contains(['/', '\\', '?', '#']) || name == "." || name == ".." {
        return Err(
            "workflow must be a filename, .github/workflows/filename, or numeric ID".into(),
        );
    }
    Ok(name)
}

pub fn repository_name(value: &str) -> Result<String, String> {
    let pieces: Vec<_> = value.split('/').collect();
    if pieces.len() != 2
        || pieces.iter().any(|p| {
            p.is_empty()
                || *p == "."
                || *p == ".."
                || !p
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        })
    {
        return Err("monitor repository must have the form owner/repository".into());
    }
    Ok(value.to_ascii_lowercase())
}

pub fn parse(text: &str, repository: &str) -> Result<Option<Rules>, String> {
    let root: RepositoryConfig = toml::from_str(text)
        // Do not log source snippets: workflow inputs can contain private data.
        .map_err(|e: toml::de::Error| format!("{CONFIG_PATH}: {}", e.message()))?;
    let Some(mut rules) = root.idle_workflows.filter(|r| r.enabled) else {
        return Ok(None);
    };
    if !(1..=43_200).contains(&rules.idle_minutes) {
        return Err("idle_minutes must be between 1 and 43200".into());
    }
    if rules.tasks.is_empty() || rules.tasks.len() > 32 || rules.monitors.len() > 32 {
        return Err("configure 1..32 tasks and at most 32 monitors".into());
    }
    let repository = repository_name(repository)?;
    let mut monitored = HashSet::new();
    for monitor in &mut rules.monitors {
        let name = repository_name(monitor.repository.as_deref().unwrap_or(&repository))?;
        if !monitored.insert(name.clone()) {
            return Err(format!("duplicate monitor for {name}"));
        }
        monitor.repository = Some(name);
        for workflow in &monitor.workflows {
            workflow_name(workflow)?;
        }
    }
    // Related repositories extend the scope; they cannot silently remove this
    // repository from its own activity/CI gate.
    if !monitored.contains(&repository) {
        rules.monitors.push(Monitor {
            repository: Some(repository),
            workflows: Vec::new(),
        });
    }
    let mut tasks = HashSet::new();
    for task in &rules.tasks {
        let workflow = workflow_name(&task.workflow)?;
        if task.branch.trim().is_empty() || task.branch.starts_with("refs/") {
            return Err("task branch must be a nonempty branch name (without refs/heads/)".into());
        }
        if !tasks.insert((workflow, &task.branch)) {
            return Err("duplicate task for the same workflow and branch".into());
        }
        if !(1..=43_200).contains(&task.retry_interval_minutes) || task.max_retries > 100 {
            return Err(
                "retry interval must be 1..43200 minutes; max_retries must be 0..100".into(),
            );
        }
        if task.inputs.len() > 25
            || task
                .inputs
                .values()
                .any(|v| !v.is_string() && !v.is_boolean() && !v.is_number())
        {
            return Err(
                "workflow inputs must be at most 25 scalar strings, booleans or numbers".into(),
            );
        }
        if !task.run_events.iter().any(|e| e == "workflow_dispatch")
            || task.run_events.iter().any(|e| e.is_empty())
        {
            return Err(
                "run_events must include workflow_dispatch and contain no empty events".into(),
            );
        }
    }
    Ok(Some(rules))
}

/// Repository configuration is TOML. Existing GitHub workflow definitions
/// necessarily remain YAML; parse them structurally, never with a text search.
pub fn validate_dispatch(text: &str, task: &Task) -> Result<(), String> {
    use serde_yaml::Value as Yaml;
    let yaml: Yaml = serde_yaml::from_str(text).map_err(|_| "invalid workflow YAML")?;
    let trigger = &yaml["on"];
    let dispatch = match trigger {
        Yaml::String(s) if s == "workflow_dispatch" => Yaml::Null,
        Yaml::Sequence(events)
            if events
                .iter()
                .any(|v| v.as_str() == Some("workflow_dispatch")) =>
        {
            Yaml::Null
        }
        Yaml::Mapping(events) => events
            .get(Yaml::String("workflow_dispatch".into()))
            .cloned()
            .ok_or("workflow does not support workflow_dispatch")?,
        _ => return Err("workflow does not support workflow_dispatch".into()),
    };
    let definitions = dispatch["inputs"].as_mapping();
    for key in task.inputs.keys() {
        if !definitions.is_some_and(|m| m.contains_key(Yaml::String(key.clone()))) {
            return Err(format!("unknown workflow input {key}"));
        }
    }
    if let Some(definitions) = definitions {
        for (key, definition) in definitions {
            let key = key.as_str().ok_or("workflow input name is not a string")?;
            let supplied = task.inputs.get(key);
            if supplied.is_none() {
                if definition["required"].as_bool() == Some(true) && definition["default"].is_null()
                {
                    return Err(format!("missing required workflow input {key}"));
                }
                continue;
            }
            let value = supplied.unwrap();
            let valid = match definition["type"].as_str().unwrap_or("string") {
                "boolean" => value.is_boolean() || matches!(value.as_str(), Some("true" | "false")),
                "number" => {
                    value.is_number()
                        || value
                            .as_str()
                            .is_some_and(|v| v.parse::<f64>().is_ok_and(f64::is_finite))
                }
                "choice" => definition["options"].as_sequence().is_some_and(|options| {
                    options.iter().any(|option| {
                        option.as_str().is_some() && option.as_str() == value.as_str()
                    })
                }),
                "string" | "environment" => value.is_string(),
                _ => false,
            };
            if !valid {
                return Err(format!("invalid type or choice for workflow input {key}"));
            }
        }
    }
    Ok(())
}
