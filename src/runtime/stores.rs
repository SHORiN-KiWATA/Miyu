//! 会话库按人分(09-10 阶段 8):管理员一份(根布局 `home/<admin>/conversation.db`,
//! 终端/语音/遗留都在里面),每个成员自己一份(`home/<用户>/conversation.db`)。
//!
//! Web 层拿到登录身份就问「他的库」;actor 与事件流手里只有 session id,就
//! 问「这个会话在谁的库里」——先查管理员的,再查开过的成员库,最后按账号表
//! 把没开过的库开一遍。会话 id 全局唯一(时间戳 + 随机),查到即定,结果缓存。
//! 账号表、邀请码、用量账本仍在管理员那份/state 目录,不分家。

use super::*;
use crate::paths::MiyuPaths;
use crate::state::StateStore;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct StoreRegistry {
    admin: StateStore,
    paths: MiyuPaths,
    /// 账号 id → 成员库
    members: Arc<Mutex<HashMap<String, StateStore>>>,
    /// 会话 id → 账号 id(空串 = 管理员)
    owners: Arc<Mutex<HashMap<String, String>>>,
}

impl StoreRegistry {
    pub(crate) fn new(admin: StateStore, paths: MiyuPaths) -> Self {
        Self {
            admin,
            paths,
            members: Arc::new(Mutex::new(HashMap::new())),
            owners: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(crate) fn admin(&self) -> &StateStore {
        &self.admin
    }

    /// 某个账号的库;空串 = 管理员。成员库第一次用时打开(建目录、跑迁移)。
    pub(crate) fn for_owner(&self, owner: &str) -> Result<StateStore> {
        if owner.is_empty() {
            return Ok(self.admin.clone());
        }
        if let Some(store) = self.members.lock().unwrap().get(owner) {
            return Ok(store.clone());
        }
        let account = self
            .admin
            .account_by_id(owner)?
            .with_context(|| format!("unknown account {owner}"))?;
        let store = StateStore::open_member(&self.paths, &account.username)?;
        let _ = store.recover_stale_turns();
        let mut members = self.members.lock().unwrap();
        Ok(members.entry(owner.to_string()).or_insert(store).clone())
    }

    pub(crate) fn for_identity(&self, identity: &WebIdentity) -> Result<StateStore> {
        self.for_owner(identity.owner_key())
    }

    /// 会话在谁的库里:找不到就当管理员的(旧路径的 404 语义不变)。
    pub(crate) fn owner_of_session(&self, session_id: &str) -> Option<String> {
        if let Some(owner) = self.owners.lock().unwrap().get(session_id) {
            return Some(owner.clone());
        }
        let owner = self.locate_session(session_id)?;
        let mut owners = self.owners.lock().unwrap();
        if owners.len() > 8_192 {
            owners.clear();
        }
        owners.insert(session_id.to_string(), owner.clone());
        Some(owner)
    }

    pub(crate) fn for_session(&self, session_id: &str) -> StateStore {
        match self.owner_of_session(session_id) {
            Some(owner) => self
                .for_owner(&owner)
                .unwrap_or_else(|_| self.admin.clone()),
            None => self.admin.clone(),
        }
    }

    fn locate_session(&self, session_id: &str) -> Option<String> {
        if matches!(self.admin.session_record(session_id), Ok(Some(_))) {
            return Some(String::new());
        }
        let opened: Vec<(String, StateStore)> = self
            .members
            .lock()
            .unwrap()
            .iter()
            .map(|(owner, store)| (owner.clone(), store.clone()))
            .collect();
        for (owner, store) in &opened {
            if matches!(store.session_record(session_id), Ok(Some(_))) {
                return Some(owner.clone());
            }
        }
        // 没开过的成员库:按账号表逐个开(只在冷启动后第一次碰到别人的会话时)
        let accounts = self.admin.list_accounts().ok()?;
        for account in accounts {
            if account.is_admin() || opened.iter().any(|(owner, _)| owner == &account.id) {
                continue;
            }
            let Ok(store) = self.for_owner(&account.id) else {
                continue;
            };
            if matches!(store.session_record(session_id), Ok(Some(_))) {
                return Some(account.id);
            }
        }
        None
    }

    /// 新建会话后登记归属,免得马上又扫一遍。
    pub(crate) fn note_session_owner(&self, session_id: &str, owner: &str) {
        self.owners
            .lock()
            .unwrap()
            .insert(session_id.to_string(), owner.to_string());
    }
}
