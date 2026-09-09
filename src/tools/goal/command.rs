//! `/goal` 命令与 WebUI 状态行的读侧。
//!
//! REPL 与 WebUI 同源于 `execute_goal_command`：命令的语法、拒绝文案、以及
//! 「armed 只在 daemon 内存里」这条，两个界面必须是同一份实现。

use super::runtime::{is_armed, set_armed};
use crate::paths::MiyuPaths;
use crate::state::{GoalPhase, GoalRecord};
use anyhow::{bail, Result};
use serde_json::{json, Value};

/// 没有目标时给的那句话。说清楚它能做什么，而不是罗列语法。
const GOAL_USAGE: &str = concat!(
    "/goal <一件要做很久的事> —— 交代之后它会自己一轮轮做下去\n",
    "跑起来之后：/goal 看进度 · /goal pause 暂停 · /goal resume 继续 · /goal clear 丢掉"
);

/// 目标的结构化快照，给 WebUI 的状态行用。
///
/// 和喂给模型的 `goal_value` 分开：那份带 CAS 凭证（id/revision），界面不需要
/// 也不该拿——它做操作走 `/goal` 命令，凭证在 daemon 侧解析。
pub fn session_goal_json(paths: &MiyuPaths, session_id: &str) -> Value {
    let Ok(store) = super::store(paths) else {
        return json!({ "goal": null });
    };
    match store.goal(session_id) {
        Ok(Some(goal)) => json!({
            "goal": {
                "objective": goal.objective,
                "phase": goal.phase.as_str(),
                "rounds_started": goal.rounds_started,
                "max_rounds": goal.max_rounds,
                "armed": is_armed(session_id),
                "blocked_code": goal.blocked_code,
                "blocked_message": goal.blocked_message,
            }
        }),
        _ => json!({ "goal": null }),
    }
}

/// 目标状态的人类可读形态。
///
/// 只说三件事：目标是什么、什么状态、跑了几轮。**不**说「自动续跑：已武装」
/// ——那是实现细节（一个进程内存里的布尔），用户要知道的是「它还会不会自己
/// 往前跑」，而这一点由「已暂停 / 受阻 / 已完成」加一句怎么恢复就说清了。
/// 轮数上限也不显示：256 是个防跑飞的兜底，不是进度条的分母。
fn render_goal_human(title: &str, goal: &GoalRecord, session_id: &str) -> String {
    let phase = match goal.phase {
        GoalPhase::Active => {
            // active 但没武装：目标还在，只是不会自己跑了（被打断过或重启过）。
            if is_armed(session_id) {
                "进行中"
            } else {
                "已停下"
            }
        }
        GoalPhase::Paused => "已暂停",
        GoalPhase::Blocked => "受阻",
        GoalPhase::Complete => "已完成",
    };
    let mut lines = vec![title.to_string(), goal.objective.clone()];
    // 「第 0 轮」只会让人怀疑计数坏了：还没开轮就只说状态。
    if goal.rounds_started > 0 {
        lines.push(format!("{phase} · 第 {} 轮", goal.rounds_started));
    } else {
        lines.push(phase.to_string());
    }
    if let Some(message) = &goal.blocked_message {
        lines.push(format!("卡在：{message}"));
    }
    let resumable = goal.phase != GoalPhase::Complete
        && (goal.phase != GoalPhase::Active || !is_armed(session_id));
    if resumable {
        lines.push("/goal resume 继续推进".to_string());
    }
    lines.join("\n")
}

/// `/goal [<objective>|clear|edit <objective>|pause|resume]`
///
/// 返回的文本直接打印给人看，**永远不进模型历史**——命令是人和 daemon 之间的
/// 对话，模型该看到的是目标本身（它自己调 `goal` action=get）。返回 `Result` 是给
/// web 层的：它要靠成败决定后续动作（比如 edit 成功后给正在跑的续轮排变更
/// 通知），拒绝文案和成功回执混成一个字符串就判不了。
pub fn try_execute_goal_command(paths: &MiyuPaths, session_id: &str, raw: &str) -> Result<String> {
    let store = super::store(paths)?;
    let raw = raw.trim();
    let (verb, rest) = match raw.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb.trim(), rest.trim()),
        None => (raw, ""),
    };
    match verb {
        "" => match store.goal(session_id)? {
            None => Ok(format!("本会话没有目标。\n{GOAL_USAGE}")),
            // 查看进度时顺带一行速查：动词就五个，别逼人去翻 /help。
            Some(goal) => Ok(format!(
                "{}\n/goal <新目标> · edit <改目标> · pause · resume · clear",
                render_goal_human("当前目标", &goal, session_id)
            )),
        },
        "clear" => {
            let Some(goal) = store.goal(session_id)? else {
                bail!("本会话没有目标");
            };
            store.clear_goal(session_id, &goal.goal_id, goal.revision)?;
            set_armed(session_id, false);
            Ok("目标已清除".to_string())
        }
        "pause" => {
            let Some(goal) = store.goal(session_id)? else {
                bail!("本会话没有目标");
            };
            let goal = store.pause_goal(session_id, &goal.goal_id, goal.revision)?;
            set_armed(session_id, false);
            Ok(render_goal_human("已暂停", &goal, session_id))
        }
        "resume" => {
            let Some(goal) = store.goal(session_id)? else {
                bail!("本会话没有目标");
            };
            let goal = store.resume_goal(session_id, &goal.goal_id, goal.revision)?;
            // 重新武装：daemon 重启后自动续跑一律失效，这里是人重新授权的入口。
            set_armed(session_id, true);
            Ok(render_goal_human("已恢复", &goal, session_id))
        }
        "edit" => {
            let Some(current) = store.goal(session_id)? else {
                bail!("本会话没有目标");
            };
            if rest.is_empty() {
                // 光敲 `/goal edit` 时给的是「怎么用」，不是整段用法——
                // 用户已经知道有 edit 了，缺的只是那个参数。顺带把当前目标
                // 贴出来，改的时候可以照着抄。
                bail!("/goal edit <新目标>\n当前：{}", current.objective);
            }
            // 已完成的目标改文案 = 开一件新的活。原地改会留下一个「已完成」
            // 却挂着新文案的目标，状态行不显示、驱动器也不推，看着像没生效。
            if current.phase == GoalPhase::Complete {
                store.clear_goal(session_id, &current.goal_id, current.revision)?;
                let goal = store.create_goal(session_id, rest, None)?;
                set_armed(session_id, true);
                return Ok(render_goal_human("目标已设定", &goal, session_id));
            }
            let goal = store.edit_goal(
                session_id,
                &current.goal_id,
                current.revision,
                Some(rest),
                None,
            )?;
            Ok(render_goal_human("已更新", &goal, session_id))
        }
        _ => {
            // 其余一律当作「新目标的文案」——`/goal 把测试跑绿` 是最常用的一条，
            // 不该要求再敲一个动词。
            if let Some(current) = store
                .goal(session_id)?
                .filter(|goal| goal.phase != GoalPhase::Complete)
            {
                // 目标停着（被打断/暂停/受阻）时，用户敲这条八成是想让它
                // 接着干——把 resume 摆在他面前，而不只是 edit/clear。
                let stalled = current.phase != GoalPhase::Active || !is_armed(session_id);
                if stalled {
                    bail!(
                        "本会话已有未完成的目标；/goal resume 继续推进，\
                         或 /goal edit 改它、/goal clear 清掉"
                    );
                }
                bail!("本会话已有未完成的目标；先 /goal edit 改它，或 /goal clear 清掉");
            }
            let goal = store.create_goal(session_id, raw, None)?;
            set_armed(session_id, true);
            Ok(render_goal_human("目标已设定", &goal, session_id))
        }
    }
}
