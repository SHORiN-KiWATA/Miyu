//! 账号/邀请码/会话归属(09-10 分层架构阶段 5)。

use super::shared::*;
use crate::state::*;

#[test]
fn first_account_is_admin_and_bootstrap_syncs_password() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&test_paths(temp.path())).unwrap();
    assert_eq!(store.count_accounts().unwrap(), 0);

    let admin = store
        .ensure_bootstrap_admin("first-pass", "shorin")
        .unwrap();
    assert_eq!(admin.username, "shorin");
    assert!(admin.is_admin());
    assert!(store
        .authenticate_account("SHORIN", "first-pass")
        .unwrap()
        .is_some());
    // 改了 -p 就该生效:引导会把管理员密码同步成新值。
    store
        .ensure_bootstrap_admin("second-pass", "shorin")
        .unwrap();
    assert!(store
        .authenticate_account("shorin", "first-pass")
        .unwrap()
        .is_none());
    assert!(store
        .authenticate_account("shorin", "second-pass")
        .unwrap()
        .is_some());
    assert_eq!(store.count_accounts().unwrap(), 1);
    // 非法家目录名退回 admin
    let other = StateStore::new(&test_paths(&temp.path().join("other"))).unwrap();
    assert_eq!(
        other
            .ensure_bootstrap_admin("x-pass-1", "")
            .unwrap()
            .username,
        BOOTSTRAP_ADMIN_USERNAME
    );
}

#[test]
fn invite_registers_once_and_disabled_accounts_cannot_log_in() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&test_paths(temp.path())).unwrap();
    let admin = store
        .ensure_bootstrap_admin("admin-pass", "shorin")
        .unwrap();
    let (code, invite) = store.create_invite(&admin.id, None, ROLE_MEMBER).unwrap();
    assert_eq!(code.len(), 8);
    assert!(invite.used_by.is_none());

    assert!(store
        .register_with_invite(&code, "bad name", "", "alice-pass")
        .is_err());
    assert!(store
        .register_with_invite(&code, "alice", "", "short")
        .is_err());
    let alice = store
        .register_with_invite(&code.to_lowercase(), "alice", "爱丽丝", "alice-pass")
        .unwrap();
    assert_eq!(alice.role, ROLE_MEMBER);
    assert_eq!(alice.display_name, "爱丽丝");
    // 一次性:用过即失效,且不能拿来注册同名。
    assert!(store
        .register_with_invite(&code, "bob", "", "bob-pass-1")
        .is_err());
    let used = store.list_invites().unwrap();
    assert_eq!(used[0].used_by.as_deref(), Some(alice.id.as_str()));

    assert!(store
        .authenticate_account("alice", "alice-pass")
        .unwrap()
        .is_some());
    store.set_account_disabled(&alice.id, true).unwrap();
    assert!(store
        .authenticate_account("alice", "alice-pass")
        .unwrap()
        .is_none());
    store.set_account_disabled(&alice.id, false).unwrap();
    store.set_account_password(&alice.id, "alice-new").unwrap();
    assert!(store
        .authenticate_account("alice", "alice-new")
        .unwrap()
        .is_some());
}

#[test]
fn sessions_are_listed_per_owner_and_turn_stores_carry_the_owner() {
    let temp = tempfile::tempdir().unwrap();
    let store = StateStore::new(&test_paths(temp.path())).unwrap();
    store.adopt_sessions_for_persona("miyu").unwrap();
    let mine = store.create_session("miyu", "mine", "user", None).unwrap();
    let alice = store
        .create_session_for_owner("miyu", "alice's", "user", None, "acct_alice")
        .unwrap();
    assert_eq!(mine.owner, "");
    assert_eq!(alice.owner, "acct_alice");

    let admin_view = store.list_local_sessions_for_owner("miyu", "").unwrap();
    assert!(admin_view
        .iter()
        .any(|overview| overview.record.session_id == mine.session_id));
    assert!(!admin_view
        .iter()
        .any(|overview| overview.record.session_id == alice.session_id));
    let alice_view = store
        .list_local_sessions_for_owner("miyu", "acct_alice")
        .unwrap();
    assert_eq!(alice_view.len(), 1);
    assert_eq!(alice_view[0].record.session_id, alice.session_id);
    // 不带归属的老接口仍然全列(终端管理面、迁移等用)。
    assert!(store.list_local_sessions("miyu").unwrap().len() >= 2);

    // 回合 store 按会话归属记账。
    assert_eq!(store.pinned_for_turn(&mine.session_id).usage_account(), "");
    assert_eq!(
        store.pinned_for_turn(&alice.session_id).usage_account(),
        "acct_alice"
    );
}
