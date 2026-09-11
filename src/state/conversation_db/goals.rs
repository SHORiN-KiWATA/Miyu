//! 会话目标（goal）的持久状态与 CAS 状态机。
//!
//! 每会话至多一个当前目标。所有变更都要带 `(goal_id, revision)`：模型先
//! `get_goal` 读出这一对，再带着它来改；期间被别人改过就撞版本、拿到「当前是
//! X 版本 Y，重读后再试」，而不是把别人的改动无声覆盖掉。续轮驱动器认领轮次
//! 时是**双 CAS**（版本 + 轮号），并发的两个驱动器只有一个能认领同一轮。
//!
//! 「是否自动续跑」（armed）有意不在这里——它驻在 daemon 内存，重启即失，必须
//! 由人 `/goal resume` 重新授权。落库的话，一次崩溃重启就能让机器在无人看管的
//! 情况下继续自己开轮。

use crate::state::conversation_db::*;

/// 目标的生命周期阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalPhase {
    Active,
    Paused,
    Blocked,
    Complete,
}

impl GoalPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Blocked => "blocked",
            Self::Complete => "complete",
        }
    }

    fn parse(value: &str) -> Result<Self> {
        Ok(match value {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "blocked" => Self::Blocked,
            "complete" => Self::Complete,
            other => bail!("unknown goal phase in db: {other}"),
        })
    }
}

/// 当前目标的快照。
#[derive(Debug, Clone, PartialEq)]
pub struct GoalRecord {
    pub session_id: String,
    pub goal_id: String,
    pub revision: i64,
    pub objective: String,
    pub phase: GoalPhase,
    pub blocked_code: Option<String>,
    pub blocked_message: Option<String>,
    pub max_rounds: i64,
    pub rounds_started: i64,
    pub created_at: String,
    pub updated_at: String,
}

/// 变更被拒的结构化原因。
///
/// 分成这几种而不是一串字符串，是因为工具层要把它转成**模型能照着自纠**的
/// 文案：撞版本要告诉它当前版本号，阶段不对要说清是从哪个阶段来的。
#[derive(Debug, Clone, PartialEq)]
pub enum GoalDenied {
    NotFound,
    AlreadyExists {
        phase: GoalPhase,
    },
    StaleRevision {
        current_goal_id: String,
        current_revision: i64,
        current_objective: String,
        current_phase: GoalPhase,
    },
    /// 驱动器并发认领同一轮时输家拿到的。和 `StaleRevision` 分开：撞轮号时
    /// revision 明明是对的，报「版本过期」会把排查引向错误的方向。
    RoundAlreadyClaimed {
        current_round: i64,
    },
    InvalidTransition {
        from: GoalPhase,
        verb: &'static str,
    },
    InvalidInput(String),
    RoundsExhausted {
        max_rounds: i64,
    },
}

impl std::fmt::Display for GoalDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => write!(f, "no goal is currently set for this session"),
            Self::AlreadyExists { phase } => write!(
                f,
                "a goal already exists with phase {}; edit or clear it before creating another",
                phase.as_str()
            ),
            // 把当前目标全文直接给出来:撞版本的模型下一步必然要问「现在的
            // 目标是什么」,让它再花一次 get_goal 往返纯属浪费。
            Self::StaleRevision {
                current_goal_id,
                current_revision,
                current_objective,
                current_phase,
            } => write!(
                f,
                "stale goal ref; the goal was changed — it is now {current_goal_id} \
                 revision {current_revision}, phase {}, objective {:?}. Retry with these \
                 exact values",
                current_phase.as_str(),
                current_objective
            ),
            Self::RoundAlreadyClaimed { current_round } => write!(
                f,
                "this goal round was already claimed (rounds_started is {current_round})"
            ),
            Self::InvalidTransition { from, verb } => {
                write!(f, "cannot {verb} a goal in phase {}", from.as_str())
            }
            Self::InvalidInput(message) => write!(f, "{message}"),
            Self::RoundsExhausted { max_rounds } => write!(
                f,
                "goal exhausted its {max_rounds} rounds; raise max_goal_rounds via edit \
                 before resuming"
            ),
        }
    }
}

impl std::error::Error for GoalDenied {}

/// 没指定上限时的续轮上限。
pub const DEFAULT_MAX_GOAL_ROUNDS: i64 = 0; // 0 = 不限(09-11 移除轮数限制)

const GOAL_COLUMNS: &str = "session_id, goal_id, revision, objective, phase, \
     blocked_code, blocked_message, max_rounds, rounds_started, created_at, updated_at";

fn goal_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(GoalRecord, String)> {
    let phase_raw: String = row.get(4)?;
    Ok((
        GoalRecord {
            session_id: row.get(0)?,
            goal_id: row.get(1)?,
            revision: row.get(2)?,
            objective: row.get(3)?,
            // 占位，由调用方解析 phase_raw 后覆盖：解析要返回 anyhow 错误，
            // 而这里只能返回 rusqlite 的。
            phase: GoalPhase::Active,
            blocked_code: row.get(5)?,
            blocked_message: row.get(6)?,
            max_rounds: row.get(7)?,
            rounds_started: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
        },
        phase_raw,
    ))
}

fn load_goal(conn: &Connection, session_id: &str) -> Result<Option<GoalRecord>> {
    let mut statement = conn.prepare(&format!(
        "SELECT {GOAL_COLUMNS} FROM goals WHERE session_id = ?1"
    ))?;
    let mut rows = statement.query(params![session_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let (mut record, phase_raw) = goal_from_row(row)?;
    record.phase = GoalPhase::parse(&phase_raw)?;
    Ok(Some(record))
}

/// 时间戳防倒流：挂钟往回跳时不早于上次更新。
///
/// `updated_at` 是给人看的顺序线索，一次 NTP 校时就能让它倒着走，那时「最近
/// 改的」在列表里排到前面去。
fn goal_next_timestamp(previous: &str) -> String {
    let now = Utc::now().to_rfc3339();
    if now.as_str() < previous {
        previous.to_string()
    } else {
        now
    }
}

/// 阻塞代码限定小写短横线式。
///
/// 这个码是给程序读的（比如驱动器自己写的 `round-limit`），留成自由文本的话
/// 很快就会混进模型编的整句话，之后没法按码分类。
fn kebab_code_valid(code: &str) -> bool {
    let mut chars = code.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_lowercase()
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !code.contains("--")
        && !code.ends_with('-')
}

impl ConversationDb {
    pub fn goal(&self, session_id: &str) -> Result<Option<GoalRecord>> {
        let conn = self.conn.lock().unwrap();
        load_goal(&conn, session_id)
    }

    /// 建目标（revision=1，phase=active）。
    ///
    /// 已有未完成目标就拒——一个会话同时追两个目标，续轮驱动器不知道该推哪个。
    /// 已完成的那个可以被顶掉（对话记录里仍留着它的全过程）。
    pub fn create_goal(
        &self,
        session_id: &str,
        objective: &str,
        max_rounds: Option<i64>,
    ) -> Result<GoalRecord> {
        let objective = objective.trim();
        if objective.is_empty() {
            bail!(GoalDenied::InvalidInput(
                "goal objective must be a non-empty string".to_string()
            ));
        }
        // 0 = 不限轮数(09-11 用户拍板移除轮数限制):缺省不再是 256,而是不限;
        // 负数仍非法。所有 rounds_started >= max_rounds 的判定都加了 max_rounds > 0 的前提。
        let max_rounds = max_rounds.unwrap_or(DEFAULT_MAX_GOAL_ROUNDS);
        if max_rounds < 0 {
            bail!(GoalDenied::InvalidInput(
                "max_goal_rounds must be zero (unlimited) or a positive integer".to_string()
            ));
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        if let Some(current) = load_goal(&tx, session_id)? {
            if current.phase != GoalPhase::Complete {
                bail!(GoalDenied::AlreadyExists {
                    phase: current.phase
                });
            }
            tx.execute(
                "DELETE FROM goals WHERE session_id = ?1",
                params![session_id],
            )?;
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO goals (session_id, goal_id, revision, objective, phase, \
                                max_rounds, rounds_started, created_at, updated_at)
             VALUES (?1, 'goal_'||lower(hex(randomblob(8))), 1, ?2, 'active', ?3, 0, ?4, ?4)",
            params![session_id, objective, max_rounds, now],
        )?;
        let record = load_goal(&tx, session_id)?.context("goal insert vanished")?;
        tx.commit()?;
        Ok(record)
    }

    /// CAS 前置：按 `(goal_id, revision)` 核对当前行，对不上就拒。
    fn expect_goal(
        tx: &Connection,
        session_id: &str,
        goal_id: &str,
        revision: i64,
    ) -> Result<GoalRecord> {
        let Some(current) = load_goal(tx, session_id)? else {
            bail!(GoalDenied::NotFound);
        };
        if current.goal_id != goal_id || current.revision != revision {
            bail!(GoalDenied::StaleRevision {
                current_goal_id: current.goal_id,
                current_revision: current.revision,
                current_objective: current.objective,
                current_phase: current.phase,
            });
        }
        Ok(current)
    }

    /// 改目标文案和/或轮数上限，阶段与 blocker 原样保留。
    pub fn edit_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
        objective: Option<&str>,
        max_rounds: Option<i64>,
    ) -> Result<GoalRecord> {
        if objective.is_none() && max_rounds.is_none() {
            bail!(GoalDenied::InvalidInput(
                "goal edit requires objective and/or max_goal_rounds".to_string()
            ));
        }
        if let Some(objective) = objective {
            if objective.trim().is_empty() {
                bail!(GoalDenied::InvalidInput(
                    "goal objective must be a non-empty string".to_string()
                ));
            }
        }
        if let Some(max_rounds) = max_rounds {
            if max_rounds < 0 {
                bail!(GoalDenied::InvalidInput(
                    "max_goal_rounds must be zero (unlimited) or a positive integer".to_string()
                ));
            }
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let current = Self::expect_goal(&tx, session_id, goal_id, revision)?;
        let updated_at = goal_next_timestamp(&current.updated_at);
        tx.execute(
            "UPDATE goals SET revision = revision + 1,
                 objective = COALESCE(?3, objective),
                 max_rounds = COALESCE(?4, max_rounds),
                 updated_at = ?5
             WHERE session_id = ?1 AND goal_id = ?2 AND revision = ?6",
            params![
                session_id,
                goal_id,
                objective.map(str::trim),
                max_rounds,
                updated_at,
                revision
            ],
        )?;
        let record = load_goal(&tx, session_id)?.context("goal vanished mid-edit")?;
        tx.commit()?;
        Ok(record)
    }

    /// 阶段转换的公共实现。
    ///
    /// - pause：仅 active
    /// - complete：active / paused / blocked 都行（人随时可以宣布收工）
    /// - resume：同上，但要求还有轮数余量，并清掉 blocker
    fn transition_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
        verb: &'static str,
        allowed: &[GoalPhase],
        next: GoalPhase,
        check_capacity: bool,
    ) -> Result<GoalRecord> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let current = Self::expect_goal(&tx, session_id, goal_id, revision)?;
        if !allowed.contains(&current.phase) {
            bail!(GoalDenied::InvalidTransition {
                from: current.phase,
                verb,
            });
        }
        if check_capacity && current.max_rounds > 0 && current.rounds_started >= current.max_rounds {
            bail!(GoalDenied::RoundsExhausted {
                max_rounds: current.max_rounds
            });
        }
        let updated_at = goal_next_timestamp(&current.updated_at);
        tx.execute(
            "UPDATE goals SET revision = revision + 1, phase = ?3,
                 blocked_code = NULL, blocked_message = NULL, updated_at = ?4
             WHERE session_id = ?1 AND goal_id = ?2 AND revision = ?5",
            params![session_id, goal_id, next.as_str(), updated_at, revision],
        )?;
        let record = load_goal(&tx, session_id)?.context("goal vanished mid-transition")?;
        tx.commit()?;
        Ok(record)
    }

    pub fn pause_goal(&self, session_id: &str, goal_id: &str, revision: i64) -> Result<GoalRecord> {
        self.transition_goal(
            session_id,
            goal_id,
            revision,
            "pause",
            &[GoalPhase::Active],
            GoalPhase::Paused,
            false,
        )
    }

    pub fn resume_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
    ) -> Result<GoalRecord> {
        self.transition_goal(
            session_id,
            goal_id,
            revision,
            "resume",
            &[GoalPhase::Active, GoalPhase::Paused, GoalPhase::Blocked],
            GoalPhase::Active,
            true,
        )
    }

    pub fn complete_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
    ) -> Result<GoalRecord> {
        self.transition_goal(
            session_id,
            goal_id,
            revision,
            "complete",
            &[GoalPhase::Active, GoalPhase::Paused, GoalPhase::Blocked],
            GoalPhase::Complete,
            false,
        )
    }

    /// 标记受阻：只能从 active 来，必须给出短横线码和非空说明。
    pub fn block_goal(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
        code: &str,
        message: &str,
    ) -> Result<GoalRecord> {
        if !kebab_code_valid(code) {
            bail!(GoalDenied::InvalidInput(format!(
                "goal block code must be lower-kebab-case: {code}"
            )));
        }
        let message = message.trim();
        if message.is_empty() {
            bail!(GoalDenied::InvalidInput(
                "goal block reason requires a non-empty message".to_string()
            ));
        }
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let current = Self::expect_goal(&tx, session_id, goal_id, revision)?;
        if current.phase != GoalPhase::Active {
            bail!(GoalDenied::InvalidTransition {
                from: current.phase,
                verb: "block",
            });
        }
        let updated_at = goal_next_timestamp(&current.updated_at);
        tx.execute(
            "UPDATE goals SET revision = revision + 1, phase = 'blocked',
                 blocked_code = ?3, blocked_message = ?4, updated_at = ?5
             WHERE session_id = ?1 AND goal_id = ?2 AND revision = ?6",
            params![session_id, goal_id, code, message, updated_at, revision],
        )?;
        let record = load_goal(&tx, session_id)?.context("goal vanished mid-block")?;
        tx.commit()?;
        Ok(record)
    }

    /// 删掉这个会话的目标行。全过程仍留在对话记录里。
    pub fn clear_goal(&self, session_id: &str, goal_id: &str, revision: i64) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        Self::expect_goal(&tx, session_id, goal_id, revision)?;
        tx.execute(
            "DELETE FROM goals WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 续轮驱动器认领下一轮：`rounds_started + 1`，revision 不变。
    ///
    /// 双 CAS（版本 + 轮号）：两个驱动器同时被唤醒时只有一个能认领同一轮，
    /// 另一个拿到 StaleRevision 就退出。revision 不动是有意的——认领轮次不是
    /// 「目标被改了」，模型手上的 (goal_id, revision) 仍然有效。
    pub fn begin_goal_round(
        &self,
        session_id: &str,
        goal_id: &str,
        revision: i64,
        expected_round: i64,
    ) -> Result<GoalRecord> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let current = Self::expect_goal(&tx, session_id, goal_id, revision)?;
        if current.phase != GoalPhase::Active {
            bail!(GoalDenied::InvalidTransition {
                from: current.phase,
                verb: "begin-round",
            });
        }
        if current.rounds_started != expected_round {
            bail!(GoalDenied::RoundAlreadyClaimed {
                current_round: current.rounds_started,
            });
        }
        if current.max_rounds > 0 && current.rounds_started >= current.max_rounds {
            bail!(GoalDenied::RoundsExhausted {
                max_rounds: current.max_rounds
            });
        }
        let updated_at = goal_next_timestamp(&current.updated_at);
        tx.execute(
            "UPDATE goals SET rounds_started = rounds_started + 1, updated_at = ?3
             WHERE session_id = ?1 AND goal_id = ?2",
            params![session_id, goal_id, updated_at],
        )?;
        let record = load_goal(&tx, session_id)?.context("goal vanished mid-round")?;
        tx.commit()?;
        Ok(record)
    }
}
