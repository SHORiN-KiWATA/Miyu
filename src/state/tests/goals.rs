//! 会话目标的生命周期与 CAS。
//!
//! 这一层的断言都围绕同一件事：**并发和陈旧引用不能悄悄改坏目标**。目标一旦
//! 被改错，续轮驱动器就会照着错的目标一直自动跑下去，而那正是最不该出错的地方。

use super::shared::*;
use crate::state::*;

fn db_with_session() -> (tempfile::TempDir, ConversationDb, String) {
    let temp = tempfile::tempdir().unwrap();
    let db = ConversationDb::open(&test_paths(temp.path()).state_dir).unwrap();
    let record = db
        .create_session(
            "miyu",
            "goal test",
            crate::state::USER_SESSION_KIND,
            None,
            "",
        )
        .unwrap();
    let session = record.session_id;
    (temp, db, session)
}

fn denied(error: &anyhow::Error) -> &GoalDenied {
    error
        .downcast_ref::<GoalDenied>()
        .expect("拒绝原因必须是结构化的 GoalDenied，否则工具层没法转成模型能自纠的文案")
}

#[test]
fn goal_lifecycle_walks_the_phases() {
    let (_t, db, session) = db_with_session();
    assert!(db.goal(&session).unwrap().is_none());

    let goal = db.create_goal(&session, "修好构建", None).unwrap();
    assert_eq!(goal.revision, 1);
    assert_eq!(goal.phase, GoalPhase::Active);
    assert_eq!(goal.rounds_started, 0);
    assert_eq!(goal.max_rounds, DEFAULT_MAX_GOAL_ROUNDS);

    // 每次变更都推进 revision——模型手上的旧引用因此立刻失效。
    let paused = db
        .pause_goal(&session, &goal.goal_id, goal.revision)
        .unwrap();
    assert_eq!(paused.phase, GoalPhase::Paused);
    assert_eq!(paused.revision, 2);

    let resumed = db
        .resume_goal(&session, &paused.goal_id, paused.revision)
        .unwrap();
    assert_eq!(resumed.phase, GoalPhase::Active);

    let blocked = db
        .block_goal(
            &session,
            &resumed.goal_id,
            resumed.revision,
            "needs-credentials",
            "缺少部署密钥",
        )
        .unwrap();
    assert_eq!(blocked.phase, GoalPhase::Blocked);
    assert_eq!(blocked.blocked_code.as_deref(), Some("needs-credentials"));

    // resume 会清掉 blocker：不清的话恢复之后仍然挂着一条早已解决的阻塞。
    let revived = db
        .resume_goal(&session, &blocked.goal_id, blocked.revision)
        .unwrap();
    assert_eq!(revived.phase, GoalPhase::Active);
    assert!(revived.blocked_code.is_none());
    assert!(revived.blocked_message.is_none());

    let done = db
        .complete_goal(&session, &revived.goal_id, revived.revision)
        .unwrap();
    assert_eq!(done.phase, GoalPhase::Complete);
}

/// 拿着过期的 revision 改，必须被拒并告知当前版本。
///
/// 这是模型自纠的唯一线索：它可能在读到目标之后、写回之前被人改过。无声覆盖
/// 意味着人刚按下的暂停会被模型的下一次写入抹掉。
#[test]
fn stale_revision_is_refused_with_the_current_one() {
    let (_t, db, session) = db_with_session();
    let goal = db.create_goal(&session, "把测试跑绿", None).unwrap();
    let moved = db
        .pause_goal(&session, &goal.goal_id, goal.revision)
        .unwrap();

    let error = db
        .resume_goal(&session, &goal.goal_id, goal.revision)
        .expect_err("旧 revision 必须被拒");
    match denied(&error) {
        GoalDenied::StaleRevision {
            current_goal_id,
            current_revision,
            current_objective,
            ..
        } => {
            assert_eq!(current_goal_id, &moved.goal_id);
            assert_eq!(*current_revision, moved.revision);
            // 报错要直接带上现在的目标全文，模型不必再花一次 get_goal。
            assert_eq!(current_objective, &moved.objective);
        }
        other => panic!("拒绝原因不对：{other:?}"),
    }
}

/// 同一轮只能被认领一次。
///
/// 驱动器由事件驱动，同一个会话可能被连着踢两脚；没有这道闸，两次唤醒会各自
/// 起一轮自动回合，轮号还重号。
#[test]
fn a_round_can_only_be_claimed_once() {
    let (_t, db, session) = db_with_session();
    let goal = db.create_goal(&session, "长任务", Some(5)).unwrap();

    let claimed = db
        .begin_goal_round(&session, &goal.goal_id, goal.revision, goal.rounds_started)
        .unwrap();
    assert_eq!(claimed.rounds_started, 1);
    // 认领轮次不改 revision：这不是「目标被改了」，模型手上的引用仍然有效。
    assert_eq!(claimed.revision, goal.revision);

    let error = db
        .begin_goal_round(&session, &goal.goal_id, goal.revision, goal.rounds_started)
        .expect_err("同一轮被认领两次");
    // 撞轮号不是撞版本：revision 明明是对的，报 StaleRevision 会误导排查。
    assert!(matches!(
        denied(&error),
        GoalDenied::RoundAlreadyClaimed { current_round: 1 }
    ));
}

/// 轮数耗尽后不再认领，也不允许直接 resume。
#[test]
fn exhausted_rounds_stop_the_driver() {
    let (_t, db, session) = db_with_session();
    let mut goal = db.create_goal(&session, "只给一轮", Some(1)).unwrap();
    goal = db
        .begin_goal_round(&session, &goal.goal_id, goal.revision, goal.rounds_started)
        .unwrap();
    assert_eq!(goal.rounds_started, goal.max_rounds);

    let error = db
        .begin_goal_round(&session, &goal.goal_id, goal.revision, goal.rounds_started)
        .expect_err("没余量还能认领");
    assert!(matches!(denied(&error), GoalDenied::RoundsExhausted { .. }));

    let paused = db
        .pause_goal(&session, &goal.goal_id, goal.revision)
        .unwrap();
    let error = db
        .resume_goal(&session, &paused.goal_id, paused.revision)
        .expect_err("没余量还能 resume");
    assert!(matches!(denied(&error), GoalDenied::RoundsExhausted { .. }));
}

/// 一个会话同时只能有一个未完成目标；已完成的那个可以被顶掉。
#[test]
fn only_one_unfinished_goal_per_session() {
    let (_t, db, session) = db_with_session();
    let goal = db.create_goal(&session, "第一个", None).unwrap();

    let error = db
        .create_goal(&session, "第二个", None)
        .expect_err("同时挂了两个目标，驱动器不知道该推哪个");
    assert!(matches!(denied(&error), GoalDenied::AlreadyExists { .. }));

    let done = db
        .complete_goal(&session, &goal.goal_id, goal.revision)
        .unwrap();
    assert_eq!(done.phase, GoalPhase::Complete);
    let next = db.create_goal(&session, "第二个", None).unwrap();
    assert_eq!(next.objective, "第二个");
    assert_eq!(next.revision, 1);
}

/// 阻塞码只收小写短横线式。
///
/// 这个码是给程序读的（驱动器自己会写 `round-limit`）。留成自由文本的话很快
/// 就会混进模型编的整句话，之后没法按码分类。
#[test]
fn block_codes_must_be_machine_readable() {
    let (_t, db, session) = db_with_session();
    let goal = db.create_goal(&session, "目标", None).unwrap();

    for bad in [
        "Needs Credentials",
        "needs_credentials",
        "needs--creds",
        "creds-",
        "",
    ] {
        let error = db
            .block_goal(&session, &goal.goal_id, goal.revision, bad, "说明")
            .unwrap_err();
        assert!(
            matches!(denied(&error), GoalDenied::InvalidInput(_)),
            "{bad:?} 不该被接受"
        );
    }
    // 说明不能空：只有一个码，人看不出发生了什么。
    let error = db
        .block_goal(&session, &goal.goal_id, goal.revision, "needs-creds", "   ")
        .unwrap_err();
    assert!(matches!(denied(&error), GoalDenied::InvalidInput(_)));
}

/// 目标随会话删除一起消失（外键级联），不留孤儿行。
#[test]
fn deleting_a_session_takes_its_goal() {
    let (_t, db, session) = db_with_session();
    db.create_goal(&session, "会被连坐的目标", None).unwrap();
    db.delete_session(&session).unwrap();
    assert!(db.goal(&session).unwrap().is_none());
}
