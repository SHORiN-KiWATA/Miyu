//! 群名、成员昵称、群角色、禁言状态的带 TTL 缓存。
//!
//! 这四样都要问 OneBot 端，都不常变，而渲染一条群消息可能要查好几次。没有缓存
//! 时一条消息就是四五个来回，在群活跃时直接把连接打满。
//!
//! 禁言缓存的 TTL 是**动态**的（`GROUP_MUTE_AVAILABLE_TTL` / `_UNKNOWN_TTL` /
//! 按解禁时刻收敛，上限 `GROUP_MUTE_MAX_TTL`）：查到确切解禁时间就缓存到那一
//! 刻，查不到才退回短 TTL——否则要么反复查，要么在解禁后还以为自己被禁言。

use crate::platforms::onebot::*;

pub(in crate::platforms::onebot) const GROUP_NAME_CACHE_TTL: Duration = Duration::from_secs(5 * 60);

pub(in crate::platforms::onebot) const GROUP_NAME_CACHE_CAPACITY: usize = 1024;

pub(in crate::platforms::onebot) const MENTION_NAME_CACHE_TTL: Duration =
    Duration::from_secs(10 * 60);

pub(in crate::platforms::onebot) const MENTION_NAME_CACHE_CAPACITY: usize = 4096;

pub(in crate::platforms::onebot) const MAX_MENTION_NAME_LOOKUPS: usize = 8;

pub(in crate::platforms::onebot) const MENTION_NAME_LOOKUP_TIMEOUT: Duration =
    Duration::from_secs(3);

pub(in crate::platforms::onebot) const GROUP_MUTE_AVAILABLE_TTL: Duration = Duration::from_secs(30);

pub(in crate::platforms::onebot) const GROUP_MUTE_UNKNOWN_TTL: Duration = Duration::from_secs(10);

pub(in crate::platforms::onebot) const GROUP_MUTE_WHOLE_NOTICE_TTL: Duration =
    Duration::from_secs(60);

pub(in crate::platforms::onebot) const GROUP_MUTE_MAX_TTL: Duration =
    Duration::from_secs(31 * 24 * 60 * 60);

pub(in crate::platforms::onebot) const GROUP_MUTE_CACHE_CAPACITY: usize = 1024;

pub(in crate::platforms::onebot) const GROUP_MUTE_LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

pub(in crate::platforms::onebot) const GROUP_ROLE_CACHE_TTL: Duration = Duration::from_secs(60);

pub(in crate::platforms::onebot) const GROUP_ROLE_CACHE_CAPACITY: usize = 1024;

#[derive(Debug, Clone)]
pub(in crate::platforms::onebot) struct GroupNameCacheEntry {
    pub(in crate::platforms::onebot) name: String,
    pub(in crate::platforms::onebot) expires_at: Instant,
    pub(in crate::platforms::onebot) last_used: Instant,
}

#[derive(Default)]
pub(in crate::platforms::onebot) struct GroupNameCache {
    pub(in crate::platforms::onebot) entries: HashMap<(i64, i64), GroupNameCacheEntry>,
}

impl GroupNameCache {
    pub(in crate::platforms::onebot) fn get(
        &mut self,
        key: (i64, i64),
        now: Instant,
    ) -> Option<String> {
        self.prune(now);
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = now;
        Some(entry.name.clone())
    }

    pub(in crate::platforms::onebot) fn insert(
        &mut self,
        key: (i64, i64),
        name: String,
        now: Instant,
    ) {
        self.prune(now);
        if self.entries.len() >= GROUP_NAME_CACHE_CAPACITY && !self.entries.contains_key(&key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            GroupNameCacheEntry {
                name,
                expires_at: now + GROUP_NAME_CACHE_TTL,
                last_used: now,
            },
        );
    }

    pub(in crate::platforms::onebot) fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.expires_at > now);
    }
}

pub(in crate::platforms::onebot) fn group_name_cache() -> &'static Mutex<GroupNameCache> {
    GROUP_NAME_CACHE.get_or_init(|| Mutex::new(GroupNameCache::default()))
}

/// dashboard 用:只查缓存,不打平台请求;没见过的群返回 None。
pub(crate) fn cached_group_name(account_id: i64, group_id: i64) -> Option<String> {
    group_name_cache()
        .lock()
        .ok()?
        .get((account_id, group_id), Instant::now())
}

#[derive(Debug, Clone)]
pub(in crate::platforms::onebot) struct MentionNameCacheEntry {
    pub(in crate::platforms::onebot) name: String,
    pub(in crate::platforms::onebot) expires_at: Instant,
    pub(in crate::platforms::onebot) last_used: Instant,
}

#[derive(Default)]
pub(in crate::platforms::onebot) struct MentionNameCache {
    pub(in crate::platforms::onebot) entries: HashMap<(i64, i64, String), MentionNameCacheEntry>,
}

impl MentionNameCache {
    pub(in crate::platforms::onebot) fn get(
        &mut self,
        key: &(i64, i64, String),
        now: Instant,
    ) -> Option<String> {
        self.entries.retain(|_, entry| entry.expires_at > now);
        let entry = self.entries.get_mut(key)?;
        entry.last_used = now;
        Some(entry.name.clone())
    }

    pub(in crate::platforms::onebot) fn insert(
        &mut self,
        key: (i64, i64, String),
        name: String,
        now: Instant,
    ) {
        self.entries.retain(|_, entry| entry.expires_at > now);
        if self.entries.len() >= MENTION_NAME_CACHE_CAPACITY && !self.entries.contains_key(&key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            MentionNameCacheEntry {
                name,
                expires_at: now + MENTION_NAME_CACHE_TTL,
                last_used: now,
            },
        );
    }
}

pub(in crate::platforms::onebot) fn mention_name_cache() -> &'static Mutex<MentionNameCache> {
    MENTION_NAME_CACHE.get_or_init(|| Mutex::new(MentionNameCache::default()))
}

#[derive(Debug, Clone, Copy)]
pub(in crate::platforms::onebot) struct GroupRoleCacheEntry {
    pub(in crate::platforms::onebot) role: BotGroupRole,
    pub(in crate::platforms::onebot) expires_at: Instant,
    pub(in crate::platforms::onebot) last_used: Instant,
}

#[derive(Default)]
pub(in crate::platforms::onebot) struct GroupRoleCache {
    pub(in crate::platforms::onebot) entries: HashMap<(i64, i64), GroupRoleCacheEntry>,
}

impl GroupRoleCache {
    pub(in crate::platforms::onebot) fn get(
        &mut self,
        key: (i64, i64),
        now: Instant,
    ) -> Option<BotGroupRole> {
        self.entries.retain(|_, entry| entry.expires_at > now);
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = now;
        Some(entry.role)
    }

    pub(in crate::platforms::onebot) fn insert(
        &mut self,
        key: (i64, i64),
        role: BotGroupRole,
        now: Instant,
    ) {
        self.entries.retain(|_, entry| entry.expires_at > now);
        if self.entries.len() >= GROUP_ROLE_CACHE_CAPACITY && !self.entries.contains_key(&key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            GroupRoleCacheEntry {
                role,
                expires_at: now + GROUP_ROLE_CACHE_TTL,
                last_used: now,
            },
        );
    }

    pub(in crate::platforms::onebot) fn remove_account(&mut self, account_id: i64) {
        self.entries.retain(|(id, _), _| *id != account_id);
    }
}

pub(in crate::platforms::onebot) fn group_role_cache() -> &'static Mutex<GroupRoleCache> {
    GROUP_ROLE_CACHE.get_or_init(|| Mutex::new(GroupRoleCache::default()))
}

#[derive(Debug, Clone, Copy)]
pub(in crate::platforms::onebot) struct GroupMuteCacheEntry {
    pub(in crate::platforms::onebot) availability: BotSendAvailability,
    pub(in crate::platforms::onebot) expires_at: Instant,
    pub(in crate::platforms::onebot) last_used: Instant,
}

#[derive(Default)]
pub(in crate::platforms::onebot) struct GroupMuteCache {
    pub(in crate::platforms::onebot) entries: HashMap<(i64, i64), GroupMuteCacheEntry>,
}

impl GroupMuteCache {
    pub(in crate::platforms::onebot) fn get(
        &mut self,
        key: (i64, i64),
        now: Instant,
    ) -> Option<BotSendAvailability> {
        self.prune(now);
        let entry = self.entries.get_mut(&key)?;
        entry.last_used = now;
        Some(entry.availability)
    }

    pub(in crate::platforms::onebot) fn insert(
        &mut self,
        key: (i64, i64),
        availability: BotSendAvailability,
        ttl: Duration,
        now: Instant,
    ) {
        self.prune(now);
        if self.entries.len() >= GROUP_MUTE_CACHE_CAPACITY && !self.entries.contains_key(&key) {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            key,
            GroupMuteCacheEntry {
                availability,
                expires_at: now + ttl.min(GROUP_MUTE_MAX_TTL),
                last_used: now,
            },
        );
    }

    pub(in crate::platforms::onebot) fn remove_account(&mut self, self_id: i64) {
        self.entries
            .retain(|(account_id, _), _| *account_id != self_id);
    }

    pub(in crate::platforms::onebot) fn prune(&mut self, now: Instant) {
        self.entries.retain(|_, entry| entry.expires_at > now);
    }
}

pub(in crate::platforms::onebot) fn group_mute_cache() -> &'static Mutex<GroupMuteCache> {
    GROUP_MUTE_CACHE.get_or_init(|| Mutex::new(GroupMuteCache::default()))
}

pub(in crate::platforms::onebot) fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}
