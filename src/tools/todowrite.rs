use super::{ToolRegistry, ToolSpec};
use crate::i18n::text as t;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    pub status: String,
    pub priority: String,
}

pub type TodoList = Arc<Mutex<Vec<Todo>>>;

pub fn register(registry: &mut ToolRegistry) {
    let todos: TodoList = Arc::new(Mutex::new(Vec::new()));
    register_with_state(registry, todos);
}

pub fn register_with_state(registry: &mut ToolRegistry, todos: TodoList) {
    let update_todos = Arc::clone(&todos);
    registry.register(ToolSpec::new(
        "todowrite",
        t(
            "Create or replace the full structured task list for the current coding session. Use this when initializing or rebuilding the whole list. For changing one existing item, use todoupdate instead.",
            "创建或替换当前编码会话的完整结构化任务列表。用于初始化或重建整个列表；如果只是修改单个已有任务，请使用 todoupdate。",
        ),
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": t("The full todo list. This replaces the entire list.", "完整任务列表。此操作会替换整个列表。"),
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {
                                "type": "string",
                                "description": t("Brief description of the task.", "任务简述。")
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": t("Current status of the task.", "任务当前状态。")
                            },
                            "priority": {
                                "type": "string",
                                "enum": ["high", "medium", "low"],
                                "description": t("Priority level of the task.", "任务优先级。")
                            }
                        },
                        "required": ["content", "status", "priority"]
                    }
                }
            },
            "required": ["todos"],
            "additionalProperties": false
        }),
        move |args| {
            let todos = Arc::clone(&todos);
            async move { todo_write(args, todos) }
        },
    ).writes());
    registry.register(ToolSpec::new(
        "todoupdate",
        t(
            "Atomically update the existing todo list. Use this for small changes: add, update, remove, or clear items without resending the full list.",
            "原子更新现有任务列表。用于小范围变更：新增、修改、删除或清空任务，不需要重传完整列表。",
        ),
        json!({
            "type": "object",
            "properties": {
                "updates": {
                    "type": "array",
                    "description": t("Sequential todo mutations to apply atomically.", "按顺序原子应用的任务变更。"),
                    "items": {
                        "type": "object",
                        "properties": {
                            "action": {
                                "type": "string",
                                "enum": ["add", "update", "remove", "clear"],
                                "description": t("Mutation type.", "变更类型。")
                            },
                            "index": {
                                "type": "integer",
                                "description": t("1-based target item index. For add, inserts at this position; omitted means append.", "目标任务序号，1 起始。新增时表示插入位置；省略则追加。")
                            },
                            "match_content": {
                                "type": "string",
                                "description": t("Exact content used to find the target when index is omitted.", "省略 index 时，用于定位目标任务的完整内容。")
                            },
                            "content": {
                                "type": "string",
                                "description": t("New task content for add or update.", "新增或修改后的任务内容。")
                            },
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "completed", "cancelled"],
                                "description": t("Updated task status.", "更新后的任务状态。")
                            },
                            "priority": {
                                "type": "string",
                                "enum": ["high", "medium", "low"],
                                "description": t("Updated task priority.", "更新后的任务优先级。")
                            }
                        },
                        "required": ["action"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["updates"],
            "additionalProperties": false
        }),
        move |args| {
            let todos = Arc::clone(&update_todos);
            async move { todo_update(args, todos) }
        },
    ).writes());
}

fn todo_write(args: Value, todos: TodoList) -> Result<String> {
    let items = args
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("todos array is required"))?;

    let mut list = Vec::with_capacity(items.len());
    for item in items {
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string();
        if content.is_empty() {
            anyhow::bail!("todo content must not be empty");
        }
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending")
            .to_string();
        let priority = item
            .get("priority")
            .and_then(Value::as_str)
            .unwrap_or("medium")
            .to_string();
        list.push(Todo {
            content,
            status,
            priority,
        });
    }

    let pending_count = list
        .iter()
        .filter(|t| t.status != "completed" && t.status != "cancelled")
        .count();

    let mut state = todos.lock().expect("todo state lock");
    *state = list.clone();
    drop(state);

    let display: Vec<Value> = list
        .iter()
        .map(|t| {
            json!({
                "content": t.content,
                "status": t.status,
                "priority": t.priority,
            })
        })
        .collect();

    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "operation": "write",
        "pending_count": pending_count,
        "total_count": list.len(),
        "todos": display,
    }))?)
}

fn todo_update(args: Value, todos: TodoList) -> Result<String> {
    let updates = args
        .get("updates")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("updates array is required"))?;
    if updates.is_empty() {
        anyhow::bail!("updates must not be empty");
    }

    let mut state = todos.lock().expect("todo state lock");
    let mut list = state.clone();
    for update in updates {
        let action = update
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match action {
            "add" => {
                let todo = todo_from_update(update)?;
                let insert_at = match update.get("index").and_then(Value::as_u64) {
                    Some(index) if index == 0 => anyhow::bail!("index must be 1-based"),
                    Some(index) => (index as usize - 1).min(list.len()),
                    None => list.len(),
                };
                list.insert(insert_at, todo);
            }
            "update" => {
                let idx = target_index(update, &list)?;
                if let Some(content) = update.get("content").and_then(Value::as_str) {
                    let content = content.trim();
                    if content.is_empty() {
                        anyhow::bail!("todo content must not be empty");
                    }
                    list[idx].content = content.to_string();
                }
                if let Some(status) = update.get("status").and_then(Value::as_str) {
                    validate_status(status)?;
                    list[idx].status = status.to_string();
                }
                if let Some(priority) = update.get("priority").and_then(Value::as_str) {
                    validate_priority(priority)?;
                    list[idx].priority = priority.to_string();
                }
            }
            "remove" => {
                let idx = target_index(update, &list)?;
                list.remove(idx);
            }
            "clear" => list.clear(),
            _ => anyhow::bail!("action must be add, update, remove, or clear"),
        }
    }
    *state = list.clone();
    drop(state);
    todo_output("update", &list)
}

fn todo_from_update(update: &Value) -> Result<Todo> {
    let content = update
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    if content.is_empty() {
        anyhow::bail!("content is required for add");
    }
    let status = update
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("pending");
    validate_status(status)?;
    let priority = update
        .get("priority")
        .and_then(Value::as_str)
        .unwrap_or("medium");
    validate_priority(priority)?;
    Ok(Todo {
        content,
        status: status.to_string(),
        priority: priority.to_string(),
    })
}

fn target_index(update: &Value, list: &[Todo]) -> Result<usize> {
    if let Some(index) = update.get("index").and_then(Value::as_u64) {
        if index == 0 || index as usize > list.len() {
            anyhow::bail!("index out of range");
        }
        return Ok(index as usize - 1);
    }
    let Some(content) = update.get("match_content").and_then(Value::as_str) else {
        anyhow::bail!("index or match_content is required");
    };
    let matches = list
        .iter()
        .enumerate()
        .filter(|(_, todo)| todo.content == content)
        .map(|(idx, _)| idx)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [idx] => Ok(*idx),
        [] => anyhow::bail!("no todo matches match_content"),
        _ => anyhow::bail!("match_content matches multiple todos; use index instead"),
    }
}

fn validate_status(status: &str) -> Result<()> {
    if matches!(status, "pending" | "in_progress" | "completed" | "cancelled") {
        Ok(())
    } else {
        anyhow::bail!("status must be pending, in_progress, completed, or cancelled")
    }
}

fn validate_priority(priority: &str) -> Result<()> {
    if matches!(priority, "high" | "medium" | "low") {
        Ok(())
    } else {
        anyhow::bail!("priority must be high, medium, or low")
    }
}

fn todo_output(operation: &str, list: &[Todo]) -> Result<String> {
    let pending_count = list
        .iter()
        .filter(|t| t.status != "completed" && t.status != "cancelled")
        .count();
    let display: Vec<Value> = list
        .iter()
        .map(|t| {
            json!({
                "content": t.content,
                "status": t.status,
                "priority": t.priority,
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(&json!({
        "ok": true,
        "operation": operation,
        "pending_count": pending_count,
        "total_count": list.len(),
        "todos": display,
    }))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_full_list() {
        let todos: TodoList = Arc::new(Mutex::new(Vec::new()));
        let result = todo_write(
            json!({
                "todos": [
                    {"content": "task A", "status": "completed", "priority": "high"},
                    {"content": "task B", "status": "in_progress", "priority": "medium"},
                    {"content": "task C", "status": "pending", "priority": "low"},
                ]
            }),
            Arc::clone(&todos),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["ok"], true);
        assert_eq!(data["total_count"], 3);
        assert_eq!(data["pending_count"], 2);
        assert_eq!(todos.lock().unwrap().len(), 3);
    }

    #[test]
    fn empty_list_clears_all() {
        let todos: TodoList = Arc::new(Mutex::new(vec![Todo {
            content: "old".into(),
            status: "pending".into(),
            priority: "low".into(),
        }]));
        let result = todo_write(
            json!({"todos": []}),
            Arc::clone(&todos),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["total_count"], 0);
        assert_eq!(data["pending_count"], 0);
        assert!(todos.lock().unwrap().is_empty());
    }

    #[test]
    fn empty_content_rejected() {
        let todos: TodoList = Arc::new(Mutex::new(Vec::new()));
        let result = todo_write(
            json!({"todos": [{"content": "  ", "status": "pending", "priority": "low"}]}),
            Arc::clone(&todos),
        );
        assert!(result.is_err());
    }

    #[test]
    fn update_status_by_index_is_atomic() {
        let todos: TodoList = Arc::new(Mutex::new(vec![
            Todo {
                content: "task A".into(),
                status: "pending".into(),
                priority: "high".into(),
            },
            Todo {
                content: "task B".into(),
                status: "in_progress".into(),
                priority: "medium".into(),
            },
        ]));
        let result = todo_update(
            json!({"updates": [{"action": "update", "index": 1, "status": "completed"}]}),
            Arc::clone(&todos),
        )
        .unwrap();
        let data: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(data["operation"], "update");
        let list = todos.lock().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].content, "task A");
        assert_eq!(list[0].status, "completed");
        assert_eq!(list[1].content, "task B");
        assert_eq!(list[1].status, "in_progress");
    }

    #[test]
    fn update_adds_and_removes_without_resending_full_list() {
        let todos: TodoList = Arc::new(Mutex::new(vec![Todo {
            content: "keep".into(),
            status: "pending".into(),
            priority: "low".into(),
        }]));
        todo_update(
            json!({"updates": [
                {"action": "add", "content": "new task", "status": "in_progress", "priority": "high"},
                {"action": "remove", "match_content": "keep"}
            ]}),
            Arc::clone(&todos),
        )
        .unwrap();
        let list = todos.lock().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].content, "new task");
        assert_eq!(list[0].status, "in_progress");
        assert_eq!(list[0].priority, "high");
    }

    #[test]
    fn update_rejects_ambiguous_content_match() {
        let todos: TodoList = Arc::new(Mutex::new(vec![
            Todo {
                content: "same".into(),
                status: "pending".into(),
                priority: "low".into(),
            },
            Todo {
                content: "same".into(),
                status: "pending".into(),
                priority: "low".into(),
            },
        ]));
        let result = todo_update(
            json!({"updates": [{"action": "update", "match_content": "same", "status": "completed"}]}),
            Arc::clone(&todos),
        );
        assert!(result.is_err());
    }
}
