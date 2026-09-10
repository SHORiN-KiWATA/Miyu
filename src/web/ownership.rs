//! 事件流的归属过滤(09-10 分层架构阶段 5,多用户)。
//!
//! 事件总线是全局一条:所有会话的增量、工具进度、队列变动都在里面。每个
//! 登录者只该收到自己名下会话的事件——不然成员能在 SSE 里看见管理员正在
//! 打的字。判定链:载荷里的 `session_id` → 归属;没有就用 `run_id` 找到
//! 会话(`run.started` 带两者,连上之后先见到它;半路接入的从活跃回合表查);
//! 两者都没有的全局事件(提问代理、任务启动)只给管理员。

use crate::web::*;

pub(in crate::web) struct EventOwnerFilter {
    state: DaemonState,
    owner: String,
    admin: bool,
    /// session_id → 是否归本人。会话归属不会变,缓存永不失效。
    sessions: HashMap<String, bool>,
    /// run_id → 是否归本人。回合结束后条目留着也无妨:run_id 不复用。
    runs: HashMap<String, bool>,
}

impl EventOwnerFilter {
    pub(in crate::web) fn new(state: DaemonState, identity: &WebIdentity) -> Self {
        Self {
            state,
            owner: identity.owner_key().to_string(),
            admin: identity.admin,
            sessions: HashMap::new(),
            runs: HashMap::new(),
        }
    }

    fn session_allowed(&mut self, session_id: &str) -> bool {
        if let Some(allowed) = self.sessions.get(session_id) {
            return *allowed;
        }
        // 会话在谁的库里(阶段 8 按人分库),就是谁的。
        let allowed = match self.state.stores.owner_of_session(session_id) {
            Some(owner) => owner == self.owner,
            // 查不到的会话(刚删/平台会话):管理员放行,成员不给。
            None => self.admin,
        };
        if self.sessions.len() > 4_096 {
            self.sessions.clear();
        }
        self.sessions.insert(session_id.to_string(), allowed);
        allowed
    }

    fn run_allowed(&mut self, run_id: &str, session_hint: Option<&str>) -> bool {
        if let Some(allowed) = self.runs.get(run_id) {
            return *allowed;
        }
        let session_id = session_hint.map(str::to_string).or_else(|| {
            self.state.manager.lock().ok().and_then(|manager| {
                manager
                    .active_runs
                    .get(run_id)
                    .map(|info| info.session_id.to_string())
            })
        });
        let allowed = match session_id {
            Some(session_id) => self.session_allowed(&session_id),
            None => self.admin,
        };
        if self.runs.len() > 4_096 {
            self.runs.clear();
        }
        self.runs.insert(run_id.to_string(), allowed);
        allowed
    }

    pub(in crate::web) fn allows(&mut self, record: &EventRecord) -> bool {
        if record.kind == "resync_required" {
            return true;
        }
        let data: Value = match serde_json::from_str(&record.data) {
            Ok(data) => data,
            Err(_) => return self.admin,
        };
        let session_id = data.get("session_id").and_then(Value::as_str);
        let run_id = data.get("run_id").and_then(Value::as_str);
        match (run_id, session_id) {
            (Some(run_id), hint) => self.run_allowed(run_id, hint),
            (None, Some(session_id)) => self.session_allowed(session_id),
            // 排序广播只带 id 列表:列表里有一条归自己就放行(前端只重排它认识的)。
            (None, None) if record.kind == "session.reordered" => {
                let ids = data
                    .get("session_ids")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                ids.iter()
                    .filter_map(Value::as_str)
                    .any(|id| self.session_allowed(id))
            }
            (None, None) => self.admin,
        }
    }
}
