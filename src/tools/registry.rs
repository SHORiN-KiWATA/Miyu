use crate::llm::{FunctionDefinition, ToolDefinition};
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub type ToolFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;
pub type ToolHandler = Arc<dyn Fn(Value) -> ToolFuture + Send + Sync>;

#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    handler: ToolHandler,
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
            handler: Arc::new(move |args| Box::pin(handler(args))),
        }
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

    async fn call(&self, args: Value) -> Result<String> {
        (self.handler)(args).await
    }
}

#[derive(Default, Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, ToolSpec>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: ToolSpec) {
        self.tools.entry(tool.name.clone()).or_insert(tool);
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(ToolSpec::definition).collect()
    }

    pub fn definitions_except(&self, excluded: &[&str]) -> Vec<ToolDefinition> {
        self.tools
            .values()
            .filter(|tool| !excluded.iter().any(|name| *name == tool.name))
            .map(ToolSpec::definition)
            .collect()
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
        tool.call(args).await
    }
}

pub fn empty_parameters() -> Value {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false,
    })
}
