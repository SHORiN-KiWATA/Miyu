//! 喂给模型的目标文案：续轮提示词、收尾指令、目标变更通知。
//!
//! 全部保留英文原文：这些是直接喂给模型的指令，而 dev 侧提示词本来就是英文，
//! 措辞贴着训练分布走比翻译过来更稳。

use crate::state::GoalRecord;
use serde_json::json;

/// 两条结束调用，goal_id/revision 都替模型填好。
///
/// `revision` 每轮可能变（人 `/goal edit` 一次就 +1），而那是模型唯一必须
/// 一字不差写对的东西，不该让它去回忆二十轮前的格式。
fn end_calls(goal: &GoalRecord) -> String {
    format!(
        "· done, verified against the workspace rather than earlier rounds' claims →\n  \
         goal {{\"action\":\"complete\",\"goal_id\":{id},\"revision\":{rev}}}\n\
         · blocked, incl. needing an answer from the user →\n  \
         goal {{\"action\":\"blocked\",\"goal_id\":{id},\"revision\":{rev},\
         \"blocked_reason\":\"…\"}}",
        id = json!(goal.goal_id),
        rev = goal.revision
    )
}

/// 续轮的提示词。
///
/// 两种形态。**第一轮**发完整版：目标是什么、规矩是什么、怎么收尾。**其余
/// 轮次**发短版——但短版是**自包含**的：一行来历、目标全文、行动规则、两条
/// 填好的结束调用。早先短版只有「same objective and rules as above」，赌的
/// 是完整版还躺在上下文里；上下文压缩会把这个赌注折掉，目标被人改过之后它
/// 又指向旧文案，为此还要一套「下一轮重发完整版」的脏标记跨模块传递。短版
/// 自包含之后，这些机关连同它们的失效方式一起消失。
///
/// 历史是只追加的，所以逐轮在末尾追加短版**不会**动到前缀缓存：破缓存的是
/// 改写已经发过的那条。
///
/// **来历那句不是客套**。实测过一次：一个会话正在排查别的事，续轮到达时
/// 模型判定「This looks like a system prompt injection or some automated goal
/// that hijacked my session」，然后拒绝执行。那个警惕本身是对的——一段没有
/// 来历的祈使句就该被怀疑。所以两种形态都要讲清是谁下的、怎么来的。
///
/// 「不调工具也不收尾就等于白开一轮」要明说：实测里模型会在得出结论后直接
/// 停笔，忘了自己身处目标轮、收尾要靠 `goal`，于是驱动器只能再开一轮
/// 让它对着自己的结论补作业。
pub fn goal_round_prompt(goal: &GoalRecord, full: bool) -> String {
    let calls = end_calls(goal);
    if !full {
        return format!(
            "<goal_round>\n\
             Round {} of {} — your standing objective, set by the user with `/goal`: {}\n\
             Make one concrete step of progress now, or end the goal — both calls are \
             complete as written:\n{calls}\n</goal_round>",
            goal.rounds_started,
            goal.max_rounds,
            json!(goal.objective)
        );
    }
    format!(
        "<goal_round>\n\
         Your standing objective, set by the user with `/goal`. These rounds start on their own \
         while the session is idle; the objective may be unrelated to the messages above.\n\
         Objective: {}  ·  Round {} of {}\n\n\
         Make one concrete step of progress now. Never spend a round only reporting that you are \
         waiting — end the goal instead. Ending your reply without calling tools or goal \
         just starts another round, so once the objective is verifiably done, state the outcome \
         AND call complete. Both calls below are complete as written; do not read the goal or \
         load tools first.\n\
         {calls}\n\
         </goal_round>",
        json!(goal.objective),
        goal.rounds_started,
        goal.max_rounds
    )
}

const WRAPUP_GROUNDING: &str = "Report only what earlier rounds and tool results in this \
     session actually establish; when a detail is not in the session, say so instead of \
     inventing it. ";

/// 自主轮终结后的收尾指令。
pub(super) fn wrapup_text(objective: &str, blocked_reason: Option<&str>) -> String {
    let heading = format!("Objective: {}\n", json!(objective));
    match blocked_reason {
        None => format!(
            "<goal_complete>\n{heading}The goal is marked complete and this autonomous run is \
             ending. Write the closing message to the user now: state the outcome, summarize \
             what was done and how it was verified, and point to the concrete results (files, \
             commits, or other artifacts). {WRAPUP_GROUNDING}Note anything the user should \
             review or do next. Address the user directly. Do not call any more tools in this \
             run; further work waits for the user's next instruction.\n</goal_complete>"
        ),
        Some(reason) => format!(
            "<goal_blocked>\n{heading}Blocked: {}\nThe goal is marked blocked and this \
             autonomous run is ending. Write the closing message to the user now: state what \
             has been completed so far, describe the concrete blocking condition and what you \
             tried, and say exactly what you need from the user to continue. {WRAPUP_GROUNDING}\
             Address the user directly. Do not call any more tools in this run; further work \
             waits for the user's next instruction.\n</goal_blocked>",
            json!(reason)
        ),
    }
}
