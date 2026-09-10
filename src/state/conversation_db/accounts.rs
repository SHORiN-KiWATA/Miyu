//! WebUI 账号与邀请码(09-10 分层架构阶段 5,多用户)。
//!
//! 防君子不防小人的设计:账号只解决「我和朋友的会话别混在一起」与「按人统计」,
//! 不做沙盒、不做按人额度。第一个账号是超级管理员;新账号只能凭管理员发的
//! 一次性邀请码注册。密码存 PBKDF2-HMAC-SHA256(标准库之外只用 sha2),
//! 旧的单密码(`sha256$…`)登录成功即透明升格。
//!
//! 「账号」与「principal」要分清:账号是注册过、将来有家目录的人;principal
//! 是任何说话的身份(QQ 群成员也有 principal 但没有账号)。WebUI 账号的
//! principal = 哈希(web, daemon 实例, 账号 id),复用 QQ 那套记忆隔离。

use super::ConversationDb;
use anyhow::{bail, Result};
use chrono::Utc;
use rusqlite::{params, OptionalExtension};

pub const ROLE_ADMIN: &str = "admin";
pub const ROLE_MEMBER: &str = "member";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub username: String,
    pub display_name: String,
    pub password_hash: String,
    pub role: String,
    pub disabled: bool,
    pub created_at: String,
    pub last_login_at: Option<String>,
}

impl Account {
    pub fn is_admin(&self) -> bool {
        self.role == ROLE_ADMIN
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    pub code_hash: String,
    pub created_by: String,
    pub created_at: String,
    pub expires_at: String,
    pub used_by: Option<String>,
    pub used_at: Option<String>,
    /// 注册后默认角色(member);留着给将来「邀请管理员」用。
    pub role: String,
}

const ACCOUNT_COLUMNS: &str =
    "id, username, display_name, password_hash, role, disabled, created_at, last_login_at";

fn account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: row.get("id")?,
        username: row.get("username")?,
        display_name: row.get("display_name")?,
        password_hash: row.get("password_hash")?,
        role: row.get("role")?,
        disabled: row.get::<_, i64>("disabled")? != 0,
        created_at: row.get("created_at")?,
        last_login_at: row.get("last_login_at")?,
    })
}

const INVITE_COLUMNS: &str =
    "code_hash, created_by, created_at, expires_at, used_by, used_at, role";

fn invite_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Invite> {
    Ok(Invite {
        code_hash: row.get("code_hash")?,
        created_by: row.get("created_by")?,
        created_at: row.get("created_at")?,
        expires_at: row.get("expires_at")?,
        used_by: row.get("used_by")?,
        used_at: row.get("used_at")?,
        role: row.get("role")?,
    })
}

/// 用户名规则:3–32 位,字母数字、`_`、`-`、`.`,首字符字母或数字。目录名
/// 将来直接用它(阶段 6 家目录),所以不许空白与路径字符。
pub fn validate_username(username: &str) -> Result<()> {
    let name = username.trim();
    let count = name.chars().count();
    if !(3..=32).contains(&count) {
        bail!("username must be 3 to 32 characters");
    }
    if !name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        bail!("username must start with a letter or digit");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        bail!("username may only contain letters, digits, '_', '-' and '.'");
    }
    Ok(())
}

impl ConversationDb {
    pub fn count_accounts(&self) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        Ok(conn.query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))?)
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY created_at ASC, username ASC"
        ))?;
        let rows = stmt.query_map([], account_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn account_by_id(&self, id: &str) -> Result<Option<Account>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE id = ?1"),
                params![id],
                account_from_row,
            )
            .optional()?)
    }

    pub fn account_by_username(&self, username: &str) -> Result<Option<Account>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!(
                    "SELECT {ACCOUNT_COLUMNS} FROM accounts WHERE lower(username) = lower(?1)"
                ),
                params![username.trim()],
                account_from_row,
            )
            .optional()?)
    }

    /// 第一个账号自动成为管理员;其余按 `role`。用户名不区分大小写唯一。
    pub fn create_account(
        &self,
        username: &str,
        display_name: &str,
        password_hash: &str,
        role: &str,
    ) -> Result<Account> {
        validate_username(username)?;
        if role != ROLE_ADMIN && role != ROLE_MEMBER {
            bail!("unknown account role: {role}");
        }
        let username = username.trim();
        let display_name = display_name.trim();
        let display_name = if display_name.is_empty() {
            username
        } else {
            display_name
        };
        let conn = self.conn.lock().unwrap();
        let existing: i64 = conn.query_row(
            "SELECT count(*) FROM accounts WHERE lower(username) = lower(?1)",
            params![username],
            |row| row.get(0),
        )?;
        if existing > 0 {
            bail!("username is already taken: {username}");
        }
        let total: i64 = conn.query_row("SELECT count(*) FROM accounts", [], |row| row.get(0))?;
        let role = if total == 0 { ROLE_ADMIN } else { role };
        let id = format!("acct_{}", hex::encode(rand::random::<[u8; 8]>()));
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO accounts (id, username, display_name, password_hash, role, disabled, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 0, ?6)",
            params![id, username, display_name, password_hash, role, now],
        )?;
        drop(conn);
        Ok(self.account_by_id(&id)?.expect("account row just inserted"))
    }

    pub fn set_account_password_hash(&self, id: &str, password_hash: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE accounts SET password_hash = ?2 WHERE id = ?1",
            params![id, password_hash],
        )?;
        Ok(())
    }

    pub fn set_account_disabled(&self, id: &str, disabled: bool) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE accounts SET disabled = ?2 WHERE id = ?1",
            params![id, disabled as i64],
        )?;
        Ok(())
    }

    pub fn set_account_display_name(&self, id: &str, display_name: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE accounts SET display_name = ?2 WHERE id = ?1",
            params![id, display_name.trim()],
        )?;
        Ok(())
    }

    pub fn touch_account_login(&self, id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE accounts SET last_login_at = ?2 WHERE id = ?1",
            params![id, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// 邀请码只存哈希;明文只在生成那一刻给管理员看一次。
    pub fn create_invite(
        &self,
        code_hash: &str,
        created_by: &str,
        expires_at: &str,
        role: &str,
    ) -> Result<Invite> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO invites (code_hash, created_by, created_at, expires_at, role)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                code_hash,
                created_by,
                Utc::now().to_rfc3339(),
                expires_at,
                role
            ],
        )?;
        drop(conn);
        Ok(self
            .invite_by_hash(code_hash)?
            .expect("invite row just inserted"))
    }

    pub fn invite_by_hash(&self, code_hash: &str) -> Result<Option<Invite>> {
        let conn = self.conn.lock().unwrap();
        Ok(conn
            .query_row(
                &format!("SELECT {INVITE_COLUMNS} FROM invites WHERE code_hash = ?1"),
                params![code_hash],
                invite_from_row,
            )
            .optional()?)
    }

    pub fn list_invites(&self) -> Result<Vec<Invite>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(&format!(
            "SELECT {INVITE_COLUMNS} FROM invites ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map([], invite_from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// 原子消费:未用、未过期才能标记为已用;返回是否成功。
    pub fn consume_invite(&self, code_hash: &str, used_by: &str, now: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE invites SET used_by = ?2, used_at = ?3
             WHERE code_hash = ?1 AND used_by IS NULL AND expires_at > ?3",
            params![code_hash, used_by, now],
        )?;
        Ok(changed == 1)
    }

    pub fn delete_invite(&self, code_hash: &str) -> Result<bool> {
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "DELETE FROM invites WHERE code_hash = ?1",
            params![code_hash],
        )?;
        Ok(changed == 1)
    }
}
