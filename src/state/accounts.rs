//! 账号:密码哈希、邀请码、StateStore 上的封装(阶段 5 多用户)。
//!
//! 哈希用 PBKDF2-HMAC-SHA256(自实现 HMAC,仓库里没有 hmac/pbkdf2 crate,而
//! 为「防君子」的口径再拉依赖不值)。格式 `pbkdf2-sha256$<iters>$<salt_hex>$<hash_hex>`。

use super::StateStore;
use anyhow::{bail, Result};
use chrono::{Duration, Utc};
use sha2::{Digest, Sha256};

pub use super::conversation_db::{validate_username, Account, Invite, ROLE_ADMIN, ROLE_MEMBER};

const PBKDF2_ITERATIONS: u32 = 60_000;
const PBKDF2_SCHEME: &str = "pbkdf2-sha256";
/// 引导管理员账号的用户名。用户可以之后在账号页改显示名;用户名改名是
/// 管理员操作(将来家目录要跟着搬)。
pub const BOOTSTRAP_ADMIN_USERNAME: &str = "admin";
pub const INVITE_DEFAULT_DAYS: i64 = 7;

fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        key_block[..32].copy_from_slice(&digest);
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(key_block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(key_block.map(|b| b ^ 0x5c));
    outer.update(inner);
    outer.finalize().into()
}

fn pbkdf2_sha256(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut salted = salt.to_vec();
    salted.extend_from_slice(&1u32.to_be_bytes());
    let mut u = hmac_sha256(password, &salted);
    let mut out = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (o, x) in out.iter_mut().zip(u.iter()) {
            *o ^= x;
        }
    }
    out
}

pub fn hash_password(password: &str) -> String {
    let salt: [u8; 16] = rand::random();
    let hash = pbkdf2_sha256(password.as_bytes(), &salt, PBKDF2_ITERATIONS);
    format!(
        "{PBKDF2_SCHEME}${PBKDF2_ITERATIONS}${}${}",
        hex::encode(salt),
        hex::encode(hash)
    )
}

pub fn verify_password(stored: &str, password: &str) -> bool {
    let mut parts = stored.split('$');
    match parts.next() {
        Some(PBKDF2_SCHEME) => {
            let (Some(iters), Some(salt), Some(hash)) = (parts.next(), parts.next(), parts.next())
            else {
                return false;
            };
            let (Ok(iters), Ok(salt), Ok(hash)) =
                (iters.parse::<u32>(), hex::decode(salt), hex::decode(hash))
            else {
                return false;
            };
            if iters == 0 || iters > 10_000_000 {
                return false;
            }
            let computed = pbkdf2_sha256(password.as_bytes(), &salt, iters);
            constant_time_eq(&computed, &hash)
        }
        _ => false,
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 邀请码:8 位,去掉易混字符;明文只给管理员看一次,库里存 sha256。
pub fn generate_invite_code() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
    let bytes: [u8; 8] = rand::random();
    bytes
        .iter()
        .map(|b| ALPHABET[(*b as usize) % ALPHABET.len()] as char)
        .collect()
}

pub fn invite_code_hash(code: &str) -> String {
    let normalized = code.trim().to_ascii_uppercase().replace(['-', ' '], "");
    hex::encode(Sha256::digest(normalized.as_bytes()))
}

pub fn validate_password(password: &str) -> Result<()> {
    let count = password.chars().count();
    if count < 6 {
        bail!("password must be at least 6 characters");
    }
    if count > 1_024 {
        bail!("password is too long");
    }
    Ok(())
}

impl StateStore {
    pub fn count_accounts(&self) -> Result<i64> {
        self.conv_db.count_accounts()
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        self.conv_db.list_accounts()
    }

    pub fn account_by_id(&self, id: &str) -> Result<Option<Account>> {
        self.conv_db.account_by_id(id)
    }

    pub fn account_by_username(&self, username: &str) -> Result<Option<Account>> {
        self.conv_db.account_by_username(username)
    }

    pub fn create_account(
        &self,
        username: &str,
        display_name: &str,
        password: &str,
        role: &str,
    ) -> Result<Account> {
        validate_password(password)?;
        self.conv_db
            .create_account(username, display_name, &hash_password(password), role)
    }

    pub fn set_account_password(&self, id: &str, password: &str) -> Result<()> {
        validate_password(password)?;
        self.conv_db
            .set_account_password_hash(id, &hash_password(password))
    }

    pub fn set_account_disabled(&self, id: &str, disabled: bool) -> Result<()> {
        self.conv_db.set_account_disabled(id, disabled)
    }

    pub fn set_account_display_name(&self, id: &str, display_name: &str) -> Result<()> {
        self.conv_db.set_account_display_name(id, display_name)
    }

    /// 用户名 + 密码 → 账号(禁用的当不存在)。
    pub fn authenticate_account(&self, username: &str, password: &str) -> Result<Option<Account>> {
        let Some(account) = self.conv_db.account_by_username(username)? else {
            return Ok(None);
        };
        if account.disabled || !verify_password(&account.password_hash, password) {
            return Ok(None);
        }
        self.conv_db.touch_account_login(&account.id)?;
        Ok(Some(account))
    }

    /// 引导:daemon 带着 `-p` 起来时,保证有一个管理员账号且密码就是它。
    /// 没有账号就建一个(用户名 = 家目录名,拿不到时 `admin`);已有管理员就
    /// 把密码重设成 `-p` 的值(它是属主这台机器上的机器级凭据,改了 `-p`
    /// 就该生效)。
    pub fn ensure_bootstrap_admin(&self, password: &str, username: &str) -> Result<Account> {
        let admins = self
            .conv_db
            .list_accounts()?
            .into_iter()
            .filter(|account| account.is_admin())
            .collect::<Vec<_>>();
        if let Some(admin) = admins.into_iter().next() {
            if !verify_password(&admin.password_hash, password) {
                self.conv_db
                    .set_account_password_hash(&admin.id, &hash_password(password))?;
            }
            return Ok(self
                .conv_db
                .account_by_id(&admin.id)?
                .expect("admin account exists"));
        }
        let username = if validate_username(username).is_ok() {
            username
        } else {
            BOOTSTRAP_ADMIN_USERNAME
        };
        self.create_account(username, "", password, ROLE_ADMIN)
    }

    /// 管理员生成邀请码:返回明文(只此一次)。
    pub fn create_invite(
        &self,
        created_by: &str,
        days: Option<i64>,
        role: &str,
    ) -> Result<(String, Invite)> {
        if role != ROLE_ADMIN && role != ROLE_MEMBER {
            bail!("unknown invite role: {role}");
        }
        let code = generate_invite_code();
        let expires =
            Utc::now() + Duration::days(days.unwrap_or(INVITE_DEFAULT_DAYS).clamp(1, 365));
        let invite = self.conv_db.create_invite(
            &invite_code_hash(&code),
            created_by,
            &expires.to_rfc3339(),
            role,
        )?;
        Ok((code, invite))
    }

    pub fn list_invites(&self) -> Result<Vec<Invite>> {
        self.conv_db.list_invites()
    }

    pub fn delete_invite(&self, code_hash: &str) -> Result<bool> {
        self.conv_db.delete_invite(code_hash)
    }

    /// 凭邀请码注册:校验 → 建号 → 标记邀请已用。邀请已用/过期/不存在一律
    /// 同一句话,不给枚举邀请码的线索。
    pub fn register_with_invite(
        &self,
        code: &str,
        username: &str,
        display_name: &str,
        password: &str,
    ) -> Result<Account> {
        validate_username(username)?;
        validate_password(password)?;
        let hash = invite_code_hash(code);
        let Some(invite) = self.conv_db.invite_by_hash(&hash)? else {
            bail!("invite code is invalid or expired");
        };
        let now = Utc::now().to_rfc3339();
        if invite.used_by.is_some() || invite.expires_at <= now {
            bail!("invite code is invalid or expired");
        }
        if self.conv_db.account_by_username(username)?.is_some() {
            bail!("username is already taken: {}", username.trim());
        }
        let account = self.create_account(username, display_name, password, &invite.role)?;
        if !self.conv_db.consume_invite(&hash, &account.id, &now)? {
            // 并发下被别人先用掉:账号已建,邀请却没消费——回滚账号。
            self.conv_db.set_account_disabled(&account.id, true)?;
            bail!("invite code is invalid or expired");
        }
        // 注册即登录:成员表里别显示「从未登录」。
        self.conv_db.touch_account_login(&account.id)?;
        Ok(self.conv_db.account_by_id(&account.id)?.unwrap_or(account))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231_case_2() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex::encode(mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn pbkdf2_matches_known_vector() {
        // RFC 6070 style vector for PBKDF2-HMAC-SHA256: ("password", "salt", 1)
        let out = pbkdf2_sha256(b"password", b"salt", 1);
        assert_eq!(
            hex::encode(out),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
        let out = pbkdf2_sha256(b"password", b"salt", 4096);
        assert_eq!(
            hex::encode(out),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    #[test]
    fn password_hash_round_trips_and_rejects_wrong_password() {
        let stored = hash_password("correct horse");
        assert!(stored.starts_with("pbkdf2-sha256$"));
        assert!(verify_password(&stored, "correct horse"));
        assert!(!verify_password(&stored, "wrong"));
        assert!(!verify_password("sha256$deadbeef", "anything"));
        assert!(!verify_password("garbage", "anything"));
    }

    #[test]
    fn invite_codes_normalize_before_hashing() {
        let code = generate_invite_code();
        assert_eq!(code.len(), 8);
        assert_eq!(
            invite_code_hash(&code),
            invite_code_hash(&format!(" {}-", code.to_lowercase()))
        );
    }
}
