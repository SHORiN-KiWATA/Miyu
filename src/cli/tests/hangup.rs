//! 挂断看门狗盯的是哪个 fd。
//!
//! 09-10 报：shellhook（`printf '%s' "$buffer" | miyu --shell-intercept
//! --stdin`）里 ask_question 的面板一打开，什么都不做、几秒后整个回合被取消。
//! 根因是看门狗裸 poll stdin——管道写端退出后 stdin 常驻 POLLHUP，被当成
//! 「终端没了」，5 秒后 `exit(1)`，daemon 又把一次性客户端的断线当取消。

use crate::cli::{fd_hung_up, hangup_watch_fd};

/// stdin 是终端时盯 stdin；stdin 被管道占用时盯控制终端。
#[test]
fn a_piped_stdin_moves_the_watch_to_the_controlling_terminal() {
    assert_eq!(
        hangup_watch_fd(true, Some(7)),
        Some(libc::STDIN_FILENO),
        "stdin 是终端:沿用原来的判据"
    );
    assert_eq!(
        hangup_watch_fd(false, Some(7)),
        Some(7),
        "stdin 是管道:管道 EOF 不是挂断,盯控制终端"
    );
    assert_eq!(
        hangup_watch_fd(false, None),
        None,
        "没有控制终端就永远不判挂断,宁可留着进程也不误杀"
    );
}

/// 记下真实形态：写端关掉的管道在 poll 里就是 POLLHUP。
#[test]
fn a_pipe_with_a_closed_writer_reports_hangup() {
    let mut fds = [0; 2];
    assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
    let (read_fd, write_fd) = (fds[0], fds[1]);
    assert!(!fd_hung_up(read_fd), "写端还开着:不算挂断");
    assert_eq!(unsafe { libc::close(write_fd) }, 0);
    assert!(
        fd_hung_up(read_fd),
        "写端一关就是 POLLHUP——所以不能拿它判终端"
    );
    assert_eq!(unsafe { libc::close(read_fd) }, 0);
}
