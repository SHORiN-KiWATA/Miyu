//! v11 起：多数是加一列。
//!
//! 加列的迁移写起来几乎一样（`add_column_if_missing` 之类），放在一起比夹在
//! 大改结构中间好找。

use crate::state::migrations::*;

/// Per-session model pool override: a JSON array of
/// `{"provider_id": ..., "model": ...}` objects. NULL follows the global
/// active pool.
pub(in crate::state) fn apply_v11_session_model_override(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "sessions", "model_override", "TEXT")
}

/// v7 append-only fossilization: the transient system tail that rode behind
/// the user message in the live request (runtime stamp, trusted transport
/// context, hints, associative memory, meme reminder) is archived verbatim so
/// history replay stays a byte-exact extension of what the provider already
/// cached ("注入了就别删"). JSON array of ChatMessage values; '[]' when none.
pub(in crate::state) fn apply_v12_turn_context_messages(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "turns",
        "context_messages",
        "TEXT NOT NULL DEFAULT '[]'",
    )
}

/// Compact tail retention: a summary turn no longer swallows every visible
/// turn, so "hidden = seq <= summary_seq" stops describing the folded set.
/// The summary row records the exact turn_ids it hid (JSON array) so undo can
/// restore precisely that set. NULL on legacy rows keeps the old undo path.
pub(in crate::state) fn apply_v13_compact_hidden_turns(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "turns", "compact_hidden_json", "TEXT")
}

/// Mechanical prune (free half of context management): old turns'
/// tool_reports can be replaced by a placeholder because tool output is
/// re-derivable. The original JSON is archived here (write-once) before the
/// first rewrite so the prune is reversible and auditable.
pub(in crate::state) fn apply_v14_tool_reports_archive(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "turns", "tool_reports_archive", "TEXT")
}

/// Unix seconds of the session's most recent completed/interrupted LLM turn.
/// Drives cold-resume pruning: a session idle past the provider cache TTL
/// resumes against a cold cache, so a history rewrite at that moment costs
/// no extra misses — it only shrinks the full-price first request. NULL on
/// legacy sessions means "unknown, skip".
pub(in crate::state) fn apply_v15_session_last_request_at(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "sessions", "last_request_at", "INTEGER")
}

/// Deterministic tool footprint per turn (JSON {read, modified, memories}):
/// file paths and memory names are facts the code knows exactly, so the
/// compactor appends them to summaries itself instead of trusting the LLM to
/// not drop or misspell them. Summary rows carry the merged footprint for
/// cross-compaction accumulation.
pub(in crate::state) fn apply_v16_turn_tool_footprint(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "turns", "tool_footprint", "TEXT")
}

/// Display transcript of a finished turn (JSON: ordered text / tool-call /
/// tool-result records). The live journal tables are wiped when a turn
/// completes because they carry whole command logs; this keeps just enough,
/// in order, for the REPL to redraw a reopened session the way it looked.
pub(in crate::state) fn apply_v17_turn_replay_journal(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "turns", "replay_journal", "TEXT")
}

/// Prompt and cache-read halves of a turn's usage. `token_total` alone cannot
/// express a cache hit rate: hits are an input-side property (output tokens
/// only enter the prompt on the *next* turn), so the rate needs the prompt as
/// its denominator, not the total. Turns recorded before this migration keep
/// zeros, which read as "the provider reported no cache" and so display
/// nothing rather than a fake 0%.
pub(in crate::state) fn apply_v18_turn_cache_tokens(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "turns", "token_prompt", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(
        conn,
        "turns",
        "token_cache_read",
        "INTEGER NOT NULL DEFAULT 0",
    )
}

/// Cache-read half of a subagent run's usage. Subagent sessions already carry
/// prompt/completion/total on the session row; the cumulative cache rate needs
/// the hits too, or folding subagent prompts into the denominator would make a
/// healthy cache read as broken.
///
/// Deliberately nullable: rows written before this migration have an *unknown*
/// cache figure, not a zero one. Defaulting them to 0 would drag their prompt
/// tokens into the rate's denominator with no hits to match — on a real
/// database that turned a measured 24% into 1%. NULL keeps them in the Σ total
/// and out of the rate.
pub(in crate::state) fn apply_v19_session_cache_tokens(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "sessions", "cache_read_tokens", "INTEGER")
}

/// v20:回合的结构化工具流(JSON)。照抄 dsh 的结构保真原则:assistant 的
/// tool_calls(含模型原样 JSON 参数)与各调用的模型侧输出逐字保留,回放时
/// 还原为原生 tool_calls + role:"tool" 消息,不再压扁成系统备忘。
pub(in crate::state) fn apply_v20_turn_tool_flow(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "turns", "tool_flow", "TEXT")
}

/// 「默认会话」实为 shellhook/CLI 快捷入口专属的 lane,改叫「终端集成
/// 会话」;只重命名仍叫旧默认名的行,用户手动改过的名字不动。
pub(in crate::state) fn apply_v21_rename_default_session(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE sessions SET name = ?1
         WHERE session_id = ?2 AND name IN ('默认会话', 'Default session')",
        rusqlite::params![
            crate::i18n::text("Terminal session", "终端集成会话"),
            DEFAULT_SESSION_ID
        ],
    )?;
    Ok(())
}

/// v22 曾建 goals 表;goal 功能于 08-16 整体移除,此迁移改为空操作
/// (老库的表由 v24 落掉,新库从未建过)。
pub(in crate::state) fn apply_v22_session_goals(_conn: &Connection) -> Result<()> {
    Ok(())
}

/// 归档会话功能整体移除:解冻所有存量归档行,否则它们在没有解档入口的
/// 新版里永远不可见。列本身保留(0 值),避免无谓的表重建。
pub(in crate::state) fn apply_v23_retire_session_archiving(conn: &Connection) -> Result<()> {
    conn.execute("UPDATE sessions SET archived = 0 WHERE archived != 0", [])?;
    Ok(())
}

/// goal 功能整体移除:老库落掉 goals 表。
pub(in crate::state) fn apply_v24_retire_session_goals(conn: &Connection) -> Result<()> {
    conn.execute("DROP TABLE IF EXISTS goals", [])?;
    Ok(())
}

/// 会话手动排序:sort_key 越小越靠前。存量按「最近活跃在前」的老展示序
/// 固化(间隔 1024 留插入空间),此后顺序只随用户拖动与新建变化,活跃不再
/// 自动置顶。
pub(in crate::state) fn apply_v28_session_sort_key(conn: &Connection) -> Result<()> {
    // 幂等:上次跑到一半的残留库里列可能已存在(ALTER 没有 IF NOT EXISTS)。
    let has_column = conn
        .prepare("SELECT 1 FROM pragma_table_info('sessions') WHERE name = 'sort_key'")?
        .exists([])?;
    if !has_column {
        conn.execute(
            "ALTER TABLE sessions ADD COLUMN sort_key INTEGER NOT NULL DEFAULT 0",
            [],
        )?;
    }
    conn.execute_batch(
        "WITH ranked AS (
             SELECT session_id,
                    ROW_NUMBER() OVER (PARTITION BY persona ORDER BY updated_at DESC) AS rn
               FROM sessions
              WHERE kind = 'user'
         )
         UPDATE sessions
            SET sort_key = (SELECT rn * 1024 FROM ranked WHERE ranked.session_id = sessions.session_id)
          WHERE kind = 'user';",
    )?;
    Ok(())
}

/// v29: 用户附件可以落在磁盘上(`state/attachments/<id>/<name>`),
/// `data` 列留空;同时放开 `kind` 让视频等任意二进制以 `file` 进来。
///
/// 两条都是 CHECK/NOT NULL 约束,SQLite 改不了,只能整表重建。行数据逐列
/// 原样搬,旧的 BLOB 附件继续从 `data` 列读(读路径双兼容)。
pub(in crate::state) fn apply_v29_attachment_files(conn: &Connection) -> Result<()> {
    // 幂等:半途而废的库里新约束可能已经就位。
    let already: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
              WHERE type = 'table' AND name = 'user_attachments'
                AND sql LIKE '%''file''%'
         )",
        [],
        |row| row.get(0),
    )?;
    if already {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE user_attachments_v29 (
            attachment_id TEXT PRIMARY KEY,
            session_id    TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE,
            turn_id       TEXT REFERENCES turns(turn_id) ON DELETE CASCADE,
            prompt_id     TEXT REFERENCES queued_prompts(prompt_id) ON DELETE CASCADE,
            run_id        TEXT,
            file_name     TEXT NOT NULL,
            mime          TEXT NOT NULL,
            kind          TEXT NOT NULL CHECK (kind IN ('image', 'text', 'file')),
            size_bytes    INTEGER NOT NULL CHECK (size_bytes >= 0),
            width         INTEGER NOT NULL DEFAULT 0,
            height        INTEGER NOT NULL DEFAULT 0,
            data          BLOB,
            created_at    TEXT NOT NULL,
            CHECK (
                (turn_id IS NOT NULL) + (prompt_id IS NOT NULL) + (run_id IS NOT NULL) <= 1
            )
        );
        INSERT INTO user_attachments_v29
            (attachment_id, session_id, turn_id, prompt_id, run_id, file_name, mime,
             kind, size_bytes, width, height, data, created_at)
        SELECT attachment_id, session_id, turn_id, prompt_id, run_id, file_name, mime,
               kind, size_bytes, width, height, data, created_at
          FROM user_attachments;
        DROP TABLE user_attachments;
        ALTER TABLE user_attachments_v29 RENAME TO user_attachments;
        CREATE INDEX idx_user_attachments_session
            ON user_attachments(session_id, created_at, attachment_id);
        CREATE INDEX idx_user_attachments_turn
            ON user_attachments(turn_id, created_at, attachment_id);
        CREATE INDEX idx_user_attachments_prompt
            ON user_attachments(prompt_id, created_at, attachment_id);
        CREATE INDEX idx_user_attachments_run ON user_attachments(run_id);",
    )?;
    Ok(())
}

/// v30: 回合内追加给模型看的媒体(工具让当前多模态模型直接看的图片/
/// 视频、剪贴板图片、视觉旁路的描述文本),按 (turn, call) 落子表,历史
/// 重放时紧跟对应 tool 消息逐字节回灌——不落库就等于下一回合模型"忘了"
/// 这张图,且缓存前缀在此掰断(09-03)。
pub(in crate::state) fn apply_v30_turn_inline_media(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS turn_inline_media (
            media_id   INTEGER PRIMARY KEY,
            turn_id    TEXT NOT NULL REFERENCES turns(turn_id) ON DELETE CASCADE,
            call_id    TEXT NOT NULL,
            seq        INTEGER NOT NULL,
            kind       TEXT NOT NULL CHECK (kind IN ('image', 'video', 'text')),
            mime       TEXT NOT NULL,
            source     TEXT NOT NULL,
            data       BLOB,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_turn_inline_media_turn
            ON turn_inline_media(turn_id, call_id, seq);",
    )?;
    Ok(())
}

/// v31: followup(排队消息)随发的瞬态尾巴也要化石化。活体里 followup 正文
/// 后面跟着 `<context-images>`/图片路径等提示块,回放只重建正文,两边字节
/// 不同 ⇒ 缓存前缀在此掰断,CLI 中转线的续传链也在此失配、下一轮全量重放
/// (09-04 codex 线实证)。存 JSON 数组的 ChatMessage,口径同 `turns.context_messages`。
pub(in crate::state) fn apply_v31_queued_prompt_context_messages(conn: &Connection) -> Result<()> {
    add_column_if_missing(
        conn,
        "queued_prompts",
        "context_messages",
        "TEXT NOT NULL DEFAULT '[]'",
    )
}
