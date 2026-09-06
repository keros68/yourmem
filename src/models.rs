//! Shared normalized types.

/// A normalized message extracted from one source line.
/// One source line may yield several messages (e.g. an assistant turn with
/// text + thinking + tool_use blocks), distinguished by `ord`.
#[derive(Debug, Clone)]
pub struct NewMessage {
    pub line_no: u64,
    pub ord: u32,
    pub kind: MessageKind,
    pub content: String,
    pub timestamp: Option<String>,
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    User,
    Assistant,
    Thinking,
    ToolCall,
    ToolResult,
    System,
    Summary,
}

impl MessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Thinking => "thinking",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::System => "system",
            Self::Summary => "summary",
        }
    }
}

/// Session-level fields collected while parsing a chunk of lines.
#[derive(Debug, Default, Clone)]
pub struct SessionMetaPatch {
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// parentUuid of the first message-bearing line (lineage signal).
    pub first_parent_uuid: Option<String>,
    /// leafUuid of a leading summary line (compact-continuation signal).
    pub compact_leaf_uuid: Option<String>,
}

/// A file artifact produced by a session (extracted from file-writing tool calls).
#[derive(Debug, Clone)]
pub struct NewArtifact {
    pub path: String,
    pub tool: String,
}

/// A uuid sighted on any source line, including non-content lines
/// (attachments etc.). Lineage resolution needs the full set: a fork's
/// parent pointer often references lines we never turn into messages.
#[derive(Debug, Clone)]
pub struct UuidSighting {
    pub line_no: u64,
    pub uuid: String,
}

/// Everything an adapter extracts from a chunk of source lines.
#[derive(Debug, Default)]
pub struct ParseOutput {
    pub meta: SessionMetaPatch,
    pub messages: Vec<NewMessage>,
    pub artifacts: Vec<NewArtifact>,
    pub uuids: Vec<UuidSighting>,
    /// 本次 chunk 含"全量历史行"（zcode 的 request 快照）：ingest 侧应对整个
    /// 文件全量重导，保证跨增量 chunk 的重复历史最终一致。
    pub history_resync: bool,
    /// 本次 chunk 出现过新格式行（codex response_item）：ingest 应把该事实持久
    /// 化到 source_files，让后续不含新格式行的 chunk 也能抑制 event_msg 副本。
    pub saw_response_items: bool,
    /// 本 chunk 内压缩摘要前缀最早出现的行号（zcode，0.3.8）：压缩发生在同文件
    /// 内部，该行之前入账的消息即"压缩前原文"。ingest 对 sessions.compact_line_no
    /// 取 MIN 落库——多次压缩时保持第一次的边界（"压缩前对话"= 首次压缩前的全部）。
    pub compact_line: Option<u64>,
}
