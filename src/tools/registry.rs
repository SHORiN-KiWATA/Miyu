use crate::llm::{FunctionDefinition, ToolDefinition};
use crate::tools::tool_descriptions::{self, LoadPolicy};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub type ToolFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;
pub type ToolHandler = Arc<dyn Fn(Value, ToolProgress) -> ToolFuture + Send + Sync>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOutputStream {
    Stdout,
    Stderr,
}

#[derive(Debug)]
pub enum ToolProgressEvent {
    Message(String),
    PrepareForExternalOutput {
        ready: oneshot::Sender<()>,
    },
    CommandOutput {
        stream: CommandOutputStream,
        chunk: Vec<u8>,
    },
}

#[derive(Clone, Default)]
pub struct ToolProgress {
    sender: Option<mpsc::UnboundedSender<ToolProgressEvent>>,
}

impl ToolProgress {
    pub fn new(sender: mpsc::UnboundedSender<ToolProgressEvent>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    pub fn report(&self, message: impl Into<String>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ToolProgressEvent::Message(message.into()));
        }
    }

    pub fn report_command_output(&self, stream: CommandOutputStream, chunk: Vec<u8>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ToolProgressEvent::CommandOutput { stream, chunk });
        }
    }

    pub async fn prepare_for_external_output(&self) {
        let Some(sender) = &self.sender else {
            return;
        };
        let (ready, receiver) = oneshot::channel();
        if sender
            .send(ToolProgressEvent::PrepareForExternalOutput { ready })
            .is_ok()
        {
            let _ = receiver.await;
        }
    }
}

#[cfg(test)]
mod progress_tests {
    use super::*;

    #[tokio::test]
    async fn external_output_waits_for_renderer_acknowledgement() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let progress = ToolProgress::new(sender);
        let prepare = progress.prepare_for_external_output();
        tokio::pin!(prepare);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut prepare)
                .await
                .is_err()
        );

        let ToolProgressEvent::PrepareForExternalOutput { ready } = receiver.recv().await.unwrap()
        else {
            panic!("expected external output preparation event");
        };
        ready.send(()).unwrap();
        prepare.await;
    }
}

#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub permission: ToolPermission,
    pub display_name: Option<String>,
    pub always_loaded: bool,
    pub is_script: bool,
    pub load_policy: LoadPolicy,
    pub groups: Vec<String>,
    handler: ToolHandler,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPermission {
    ReadOnly,
    Writes,
}

impl ToolSpec {
    pub fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            permission: ToolPermission::ReadOnly,
            display_name: None,
            always_loaded: true,
            is_script: false,
            load_policy: LoadPolicy::Summary,
            groups: Vec::new(),
            handler: Arc::new(move |args, _progress| Box::pin(handler(args))),
        }
    }

    pub fn new_with_progress<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value, ToolProgress) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            permission: ToolPermission::ReadOnly,
            display_name: None,
            always_loaded: true,
            is_script: false,
            load_policy: LoadPolicy::Summary,
            groups: Vec::new(),
            handler: Arc::new(move |args, progress| Box::pin(handler(args, progress))),
        }
    }

    pub fn writes(mut self) -> Self {
        self.permission = ToolPermission::Writes;
        self
    }

    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    pub fn with_always_loaded(mut self, always_loaded: bool) -> Self {
        self.always_loaded = always_loaded;
        self
    }

    pub fn with_load_policy(mut self, load_policy: LoadPolicy) -> Self {
        self.load_policy = load_policy;
        self
    }

    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.groups = groups
            .into_iter()
            .map(|group| group.trim().to_string())
            .filter(|group| !group.is_empty())
            .collect();
        self
    }

    pub fn script(mut self) -> Self {
        self.is_script = true;
        self
    }

    pub fn apply_built_in_description(mut self) -> Self {
        if self.name == "load_skill" {
            return self;
        }
        if let Some(desc) = crate::tools::tool_descriptions::get(&self.name) {
            self.description = desc.description.clone();
            self.parameters = desc.parameters.clone();
            self.display_name = Some(desc.display_name.clone());
            self.always_loaded = desc.always_loaded;
            self.load_policy = desc.load_policy;
            self.groups = desc.groups.clone();
        }
        self
    }

    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            kind: "function",
            function: FunctionDefinition {
                name: self.name.clone(),
                description: self.description.clone(),
                parameters: self.parameters.clone(),
            },
        }
    }

    async fn call(&self, args: Value, progress: ToolProgress) -> Result<String> {
        (self.handler)(args, progress).await
    }

    fn call_future(&self, args: Value, progress: ToolProgress) -> ToolFuture {
        (self.handler)(args, progress)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnregisteredScript {
    pub name: String,
    pub path: String,
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<ToolSpec>>,
    script_tool_names: BTreeSet<String>,
    unregistered_scripts: Vec<UnregisteredScript>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: ToolSpec) {
        let tool = tool.apply_built_in_description();
        self.tools.insert(tool.name.clone(), Arc::new(tool));
    }

    pub fn replace_script_tools(
        &mut self,
        scripts: Vec<ToolSpec>,
        mut unregistered: Vec<UnregisteredScript>,
    ) -> Result<()> {
        let mut names = BTreeSet::new();
        let mut accepted = Vec::new();
        for script in scripts {
            if !script.is_script {
                bail!("script tool is missing script origin: {}", script.name);
            }
            if !names.insert(script.name.clone()) {
                bail!("duplicate script id: {}", script.name);
            }
            if script.name == "load_tools"
                || crate::tools::tool_descriptions::get(&script.name).is_some()
                || (self.tools.contains_key(&script.name)
                    && !self.script_tool_names.contains(&script.name))
            {
                continue;
            }
            accepted.push(script);
        }

        for name in &self.script_tool_names {
            self.tools.remove(name);
        }
        self.script_tool_names.clear();

        for script in accepted {
            self.script_tool_names.insert(script.name.clone());
            self.tools.insert(script.name.clone(), Arc::new(script));
        }

        unregistered.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
        self.unregistered_scripts = unregistered;
        Ok(())
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .map(|tool| tool.definition())
            .collect::<Vec<_>>();
        definitions.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        definitions
    }

    pub fn lazy_definitions(&self, loaded: &BTreeSet<String>) -> Vec<ToolDefinition> {
        let mut definitions = self
            .tools
            .values()
            .filter(|tool| tool.always_loaded || loaded.contains(&tool.name))
            .map(|tool| {
                let mut definition = tool.definition();
                if tool.name == "load_tools" {
                    definition.function.description =
                        super::load_tools::dynamic_description(self, loaded);
                }
                definition
            })
            .collect::<Vec<_>>();
        definitions.sort_by(|a, b| a.function.name.cmp(&b.function.name));
        definitions
    }

    pub fn requires_lazy_load(&self, name: &str, loaded: &BTreeSet<String>) -> bool {
        self.tools
            .get(name)
            .map(|tool| !tool.always_loaded && !loaded.contains(name))
            .unwrap_or(false)
    }

    pub fn can_auto_load_direct_call(&self, name: &str) -> bool {
        self.tools
            .get(name)
            .map(|tool| tool.load_policy == LoadPolicy::Summary && !tool.always_loaded)
            .unwrap_or(false)
    }

    pub fn definitions_except(&self, excluded: &[&str]) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .filter(|tool| !excluded.iter().any(|name| *name == tool.name))
            .map(|tool| tool.definition())
            .collect()
    }

    pub fn permission(&self, name: &str) -> Result<ToolPermission> {
        let Some(tool) = self.tools.get(name) else {
            bail!("unknown tool: {name}");
        };
        Ok(tool.permission)
    }

    pub async fn call(&self, name: &str, arguments: &str) -> Result<String> {
        let Some(tool) = self.tools.get(name) else {
            bail!("unknown tool: {name}");
        };
        let args = if arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(arguments)?
        };
        if name == "load_tools" {
            return super::load_tools::execute(args, self);
        }
        tool.call(args, ToolProgress::default()).await
    }

    pub fn call_with_progress_future(
        &self,
        name: &str,
        arguments: &str,
        sender: mpsc::UnboundedSender<ToolProgressEvent>,
    ) -> Result<ToolFuture> {
        let Some(tool) = self.tools.get(name) else {
            bail!("unknown tool: {name}");
        };
        let args = if arguments.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(arguments)?
        };
        if name == "load_tools" {
            let result = super::load_tools::execute(args, self);
            return Ok(Box::pin(async move { result }));
        }
        Ok(tool.call_future(args, ToolProgress::new(sender)))
    }

    pub fn display_name(&self, name: &str) -> Option<String> {
        self.tools.get(name).and_then(|t| t.display_name.clone())
    }

    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&ToolSpec> {
        self.tools.get(name).map(Arc::as_ref)
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    pub(crate) fn loadable_tools(&self, loaded: &BTreeSet<String>) -> Vec<&ToolSpec> {
        let mut tools = self
            .tools
            .values()
            .map(Arc::as_ref)
            .filter(|tool| {
                tool.name != "load_tools" && !tool.always_loaded && !loaded.contains(&tool.name)
            })
            .collect::<Vec<_>>();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    pub(crate) fn expand_load_targets(
        &self,
        requested: &[String],
        loaded: &BTreeSet<String>,
    ) -> Result<(Vec<String>, Vec<String>)> {
        let mut loaded_targets = BTreeSet::new();
        let mut loaded_tools = BTreeSet::new();
        for target in requested {
            let target = target.trim();
            if target.is_empty() {
                continue;
            }
            if let Some(group) = target.strip_prefix("group:") {
                let group = group.trim();
                if group.is_empty() {
                    bail!("group target is missing a group name");
                }
                let group_tools = self.group_loadable_tool_names(group, loaded);
                if group_tools.is_empty() {
                    bail!("unknown or already-loaded tool group: {group}");
                }
                loaded_targets.insert(format!("group:{group}"));
                loaded_tools.extend(group_tools);
                continue;
            }

            let Some(tool) = self.tools.get(target) else {
                bail!("unknown tool or script: {target}");
            };
            if tool.name == "load_tools" || tool.always_loaded {
                bail!(
                    "tool cannot be loaded with load_tools: {target}. Only names listed in available_load_targets can be loaded."
                );
            }
            if tool.load_policy == LoadPolicy::Hidden {
                bail!("tool is hidden from load_tools: {target}");
            }
            if !loaded.contains(&tool.name) {
                loaded_targets.insert(tool.name.clone());
                loaded_tools.insert(tool.name.clone());
            }
        }
        Ok((
            loaded_targets.into_iter().collect(),
            loaded_tools.into_iter().collect(),
        ))
    }

    fn group_loadable_tool_names(&self, group: &str, loaded: &BTreeSet<String>) -> Vec<String> {
        let mut names = self
            .tools
            .values()
            .filter(|tool| {
                tool.name != "load_tools"
                    && !tool.always_loaded
                    && !loaded.contains(&tool.name)
                    && tool.load_policy != LoadPolicy::Hidden
                    && tool.groups.iter().any(|candidate| candidate == group)
            })
            .map(|tool| tool.name.clone())
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    pub(crate) fn load_targets_xml(&self, loaded: &BTreeSet<String>) -> String {
        let loadable = self.loadable_tools(loaded);
        let mut groups: std::collections::BTreeMap<String, Vec<&ToolSpec>> =
            std::collections::BTreeMap::new();
        let mut targets = Vec::new();

        for tool in loadable {
            match tool.load_policy {
                LoadPolicy::Summary => targets.push(load_target_tool_xml(tool)),
                LoadPolicy::Group => {
                    for group in &tool.groups {
                        groups.entry(group.clone()).or_default().push(tool);
                    }
                }
                LoadPolicy::Hidden => {}
            }
        }

        for (group, mut tools) in groups {
            tools.sort_by(|a, b| a.name.cmp(&b.name));
            let members = tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let summary = tool_descriptions::group_summary(&group);
            targets.push(format!(
                "  <target>\n    <name>group:{}</name>\n    <type>group</type>\n    <summary>{}</summary>\n    <tools>{}</tools>\n  </target>",
                xml_escape(&group),
                xml_escape(&summary),
                xml_escape(&members),
            ));
        }

        format!(
            "<available_load_targets>\n{}\n</available_load_targets>",
            targets.join("\n")
        )
    }

    pub(crate) fn unregistered_scripts(&self) -> &[UnregisteredScript] {
        &self.unregistered_scripts
    }

    pub fn clone_filtered(&self, allowed: &[&str]) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        for name in allowed {
            if let Some(spec) = self.tools.get(*name) {
                registry.tools.insert(spec.name.clone(), Arc::clone(spec));
            }
        }
        registry
    }
}

pub fn empty_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
}

fn load_target_tool_xml(tool: &ToolSpec) -> String {
    let kind = if tool.is_script { "script" } else { "tool" };
    format!(
        "  <target>\n    <name>{}</name>\n    <type>{kind}</type>\n    <summary>{}</summary>\n  </target>",
        xml_escape(&tool.name),
        xml_escape(&tool.description),
    )
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn lazy_definitions_include_loaded_on_demand_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(ToolSpec::new(
            "read_file",
            "old",
            json!({"type":"object","properties":{}}),
            |_| async { Ok(String::new()) },
        ));
        registry.register(
            ToolSpec::new(
                "custom_lazy_tool",
                "old",
                json!({"type":"object","properties":{}}),
                |_| async { Ok(String::new()) },
            )
            .with_always_loaded(false),
        );

        let names = |defs: Vec<ToolDefinition>| {
            defs.into_iter()
                .map(|def| def.function.name)
                .collect::<BTreeSet<_>>()
        };

        assert!(names(registry.lazy_definitions(&BTreeSet::new())).contains("read_file"));
        assert!(!names(registry.lazy_definitions(&BTreeSet::new())).contains("custom_lazy_tool"));

        let loaded = BTreeSet::from(["custom_lazy_tool".to_string()]);
        assert!(names(registry.lazy_definitions(&loaded)).contains("custom_lazy_tool"));
    }

    #[test]
    fn lazy_gate_requires_load_for_on_demand_builtin_tools() {
        let mut registry = ToolRegistry::new();
        registry.register(
            ToolSpec::new(
                "custom_lazy_tool",
                "old",
                json!({"type":"object","properties":{}}),
                |_| async { Ok(String::new()) },
            )
            .with_always_loaded(false),
        );
        assert!(registry.requires_lazy_load("custom_lazy_tool", &BTreeSet::new()));

        let loaded = BTreeSet::from(["custom_lazy_tool".to_string()]);
        assert!(!registry.requires_lazy_load("custom_lazy_tool", &loaded));
    }

    #[test]
    fn cloned_registry_shares_immutable_tool_specs() {
        let mut registry = ToolRegistry::new();
        registry.register(ToolSpec::new(
            "shared_tool",
            "description",
            json!({"type":"object","properties":{}}),
            |_| async { Ok(String::new()) },
        ));

        let cloned = registry.clone();

        assert!(Arc::ptr_eq(
            registry.tools.get("shared_tool").unwrap(),
            cloned.tools.get("shared_tool").unwrap()
        ));
    }

    #[test]
    fn cloned_registry_keeps_snapshot_when_tool_is_replaced() {
        let mut registry = ToolRegistry::new();
        registry.register(ToolSpec::new(
            "replaceable_tool",
            "old description",
            json!({"type":"object","properties":{}}),
            |_| async { Ok(String::new()) },
        ));
        let cloned = registry.clone();

        registry.register(ToolSpec::new(
            "replaceable_tool",
            "new description",
            json!({"type":"object","properties":{}}),
            |_| async { Ok(String::new()) },
        ));

        assert_eq!(
            cloned.get("replaceable_tool").unwrap().description,
            "old description"
        );
        assert_eq!(
            registry.get("replaceable_tool").unwrap().description,
            "new description"
        );
    }
}
