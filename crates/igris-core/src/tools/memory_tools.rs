//! Persistent memory tools (v1.2, goal.md section 7): memory_save,
//! memory_recall, create_note, activity_log, reminder_set - over the
//! file-backed store in crate::memory.

use crate::memory::{self, MemoryKind, ScoredMemory};
use crate::tools::config_tools::arg_string;
use serde_json::Value;

/* ================================================================ */
/* v1.2 — persistent memory (hybrid: own sync store + vendored       */
/* mem0-rust kernels; the store itself lives in crate::memory)       */
/* ================================================================ */

/// Memory tools run against the process-wide store. Argument validation
/// happens BEFORE the store is locked, so a bad call can never touch the
/// user's memory file (this also keeps the lib tests hermetic: they only
/// exercise the validation path through `dispatch`, the store logic
/// directly, never a valid memory dispatch in-process).
fn locked_store() -> Result<std::sync::MutexGuard<'static, memory::MemoryStore>, String> {
    memory::global()
        .lock()
        .map_err(|_| "memory: store lock poisoned".to_string())
}

fn arg_tags(args: &Value) -> Result<Vec<String>, String> {
    match args.get("tags") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => {
            let mut tags = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::String(s) => tags.push(s.clone()),
                    _ => return Err("memory: 'tags' must be an array of strings".to_string()),
                }
            }
            Ok(tags)
        }
        Some(_) => Err("memory: 'tags' must be an array of strings".to_string()),
    }
}

fn arg_limit(args: &Value) -> Result<usize, String> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(5),
        Some(Value::Number(n)) => match n.as_u64() {
            Some(v @ 1..=50) => Ok(v as usize),
            _ => Err("memory: 'limit' must be an integer between 1 and 50".to_string()),
        },
        Some(_) => Err("memory: 'limit' must be an integer between 1 and 50".to_string()),
    }
}

pub(crate) fn run_memory_save(args: &Value) -> Result<String, String> {
    let content = arg_string(args, "content")?;
    if content.trim().is_empty() {
        return Err("memory: 'content' must not be empty".to_string());
    }
    let tags = arg_tags(args)?;
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Memory, &content, &tags, None)?;
    Ok(if existed {
        format!("Already saved earlier ({}): {}", record.id, record.content)
    } else {
        format!("Memory saved ({}).", record.id)
    })
}

pub(crate) fn run_memory_recall(args: &Value) -> Result<String, String> {
    let query = arg_string(args, "query")?;
    let tags = arg_tags(args)?;
    let limit = arg_limit(args)?;
    let store = locked_store()?;
    let hits = store.recall(&query, &tags, limit);
    Ok(format_recall_hits(&hits))
}

pub(crate) fn run_create_note(args: &Value) -> Result<String, String> {
    let content = arg_string(args, "content")?;
    if content.trim().is_empty() {
        return Err("memory: 'content' must not be empty".to_string());
    }
    let tags = arg_tags(args)?;
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Note, &content, &tags, None)?;
    Ok(if existed {
        format!("An identical note already exists ({}).", record.id)
    } else {
        format!("Note created ({}).", record.id)
    })
}

pub(crate) fn run_activity_log(args: &Value) -> Result<String, String> {
    let action = arg_string(args, "action")?;
    if action.trim().is_empty() {
        return Err("memory: 'action' must not be empty".to_string());
    }
    let detail = match args.get("detail") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_string(),
        Some(_) => return Err("memory: 'detail' must be a string".to_string()),
    };
    let tags = arg_tags(args)?;
    let content = if detail.is_empty() {
        action
    } else {
        format!("{action} — {detail}")
    };
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Activity, &content, &tags, None)?;
    Ok(if existed {
        format!("Already logged earlier ({}).", record.id)
    } else {
        format!("Logged activity ({}).", record.id)
    })
}

pub(crate) fn run_reminder_set(args: &Value) -> Result<String, String> {
    let content = arg_string(args, "content")?;
    if content.trim().is_empty() {
        return Err("memory: 'content' must not be empty".to_string());
    }
    let due_at = arg_string(args, "due_at")?;
    if due_at.trim().is_empty() {
        return Err("memory: 'due_at' must not be empty".to_string());
    }
    let tags = arg_tags(args)?;
    let mut store = locked_store()?;
    let (record, existed) = store.add(MemoryKind::Reminder, &content, &tags, Some(due_at))?;
    Ok(if existed {
        format!("This reminder already exists ({}), due {}.", record.id, record.due_at.unwrap_or_default())
    } else {
        format!(
            "Reminder saved ({}), due {}.",
            record.id,
            record.due_at.unwrap_or_default()
        )
    })
}

fn format_recall_hits(hits: &[ScoredMemory]) -> String {
    if hits.is_empty() {
        return "No matching records found.".to_string();
    }
    let mut out = String::from("Found:\n");
    for hit in hits {
        let record = &hit.record;
        out.push_str(&format!("- [{}] {}", record.kind.label(), record.content));
        if !record.tags.is_empty() {
            out.push_str(&format!(" ({})", record.tags.join(", ")));
        }
        if let Some(due) = &record.due_at {
            out.push_str(&format!(" — due {due}"));
        }
        out.push_str(&format!(" — saved {}", record.created_at));
        out.push('\n');
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryRecord;
    use crate::tools::seed_registry;
    use serde_json::json;

    #[test]
    fn memory_tools_reject_bad_args_before_touching_the_store() {
        let r = seed_registry();
        assert!(r.dispatch("memory_save", &json!({})).unwrap_err().contains("content"));
        assert!(r
            .dispatch("memory_save", &json!({"content": "   "}))
            .unwrap_err()
            .contains("content"));
        assert!(r
            .dispatch("memory_save", &json!({"content": "ok", "tags": "x"}))
            .unwrap_err()
            .contains("tags"));
        assert!(r.dispatch("memory_recall", &json!({})).unwrap_err().contains("query"));
        assert!(r
            .dispatch("memory_recall", &json!({"query": "x", "limit": 0}))
            .unwrap_err()
            .contains("limit"));
        assert!(r
            .dispatch("memory_recall", &json!({"query": "x", "limit": "5"}))
            .unwrap_err()
            .contains("limit"));
        assert!(r.dispatch("create_note", &json!({})).unwrap_err().contains("content"));
        assert!(r
            .dispatch("activity_log", &json!({"action": "x", "detail": 5}))
            .unwrap_err()
            .contains("detail"));
        assert!(r.dispatch("reminder_set", &json!({"content": "call mom"})).unwrap_err().contains("due_at"));
        assert!(r
            .dispatch("reminder_set", &json!({"content": "call mom", "due_at": "   "}))
            .unwrap_err()
            .contains("due_at"));
    }

    #[test]
    fn format_recall_hits_shapes_the_model_reply() {
        assert_eq!(format_recall_hits(&[]), "No matching records found.");
        let hits = vec![
            ScoredMemory {
                record: MemoryRecord {
                    id: "m1".to_string(),
                    kind: MemoryKind::Reminder,
                    content: "Call mom".to_string(),
                    tags: vec!["family".to_string()],
                    created_at: "2026-11-14 12:00:00".to_string(),
                    due_at: Some("tomorrow 9am".to_string()),
                },
                score: 1.0,
                via: "exact",
            },
            ScoredMemory {
                record: MemoryRecord {
                    id: "m2".to_string(),
                    kind: MemoryKind::Memory,
                    content: "Wifi password is hunter2".to_string(),
                    tags: Vec::new(),
                    created_at: "2026-11-14 12:30:00".to_string(),
                    due_at: None,
                },
                score: 0.5,
                via: "similar",
            },
        ];
        let out = format_recall_hits(&hits);
        assert!(out.starts_with("Found:\n"), "{out}");
        assert!(out.contains("- [reminder] Call mom (family) — due tomorrow 9am — saved 2026-11-14 12:00:00"), "{out}");
        assert!(out.contains("- [memory] Wifi password is hunter2 — saved 2026-11-14 12:30:00"), "{out}");
    }

    /* ---- schema-confused wrapper unwrapping (dispatch-time repair) ---- */

}
