//! 本机调用结果，不保存查询、正文或错误详情；旧 usage_log 仅代表调用次数。
use serde_json::{json, Value};
use std::path::Path;

const OPERATIONS: &[(&str, &str)] = &[
    ("ingest", "import"),
    ("app", "search"),
    ("cli", "search"),
    ("mcp", "search_history"),
    ("mcp", "read_session"),
    ("mcp", "get_project_context"),
    ("mcp", "save_memory"),
    ("mcp", "read_native_memory"),
];

pub fn record(home: &Path, source: &str, name: &str, ok: bool, count: Option<u64>) {
    if !OPERATIONS.contains(&(source, name)) {
        return;
    }
    let dir = home.join("recall-status");
    let path = dir.join(format!("{source}-{name}.json"));
    let now = crate::now_iso();
    let previous = std::fs::read(&path)
        .ok()
        .and_then(|s| serde_json::from_slice::<Value>(&s).ok());
    let last_success = if ok {
        json!(now)
    } else {
        previous
            .and_then(|v| v.get("last_success").cloned())
            .unwrap_or(Value::Null)
    };
    let value = json!({"source": source, "name": name, "completed_at": now,
        "ok": ok, "result_count": count, "last_success": last_success});
    // 独立文件避免改写 config.json；不同进程、线程使用独立临时文件。
    let tmp = dir.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let _ = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&tmp, serde_json::to_vec(&value)?)?;
        std::fs::rename(&tmp, &path)
    })();
    let _ = std::fs::remove_file(tmp);
}

pub fn read(home: &Path) -> Vec<Value> {
    OPERATIONS
        .iter()
        .map(|(source, name)| {
            std::fs::read(
                home.join("recall-status")
                    .join(format!("{source}-{name}.json")),
            )
            .ok()
            .and_then(|s| serde_json::from_slice::<Value>(&s).ok())
            .filter(|v| v.is_object())
            .unwrap_or_else(|| json!({"source": source, "name": name, "ok": null}))
        })
        .collect()
}
