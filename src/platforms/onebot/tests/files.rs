//! 群文件的落盘与容量。

use crate::platforms::onebot::*;

/// `get_file` 的返回三种形态都要认,优先级本地路径 > http 直链 > base64。
#[test]
fn parses_platform_file_sources_in_priority_order() {
    let data = json!({
        "file": "/home/napcat/cache/clip.mp4",
        "url": "https://cdn.example/clip.mp4",
        "base64": "aGk="
    });
    assert_eq!(
        parse_platform_file_sources(&data),
        vec![
            PlatformFileSource::LocalPath("/home/napcat/cache/clip.mp4".into()),
            PlatformFileSource::Url("https://cdn.example/clip.mp4".to_string()),
            PlatformFileSource::Bytes(b"hi".to_vec()),
        ]
    );
    // file:// 形式的 url 当本地路径;相对路径与空 base64 忽略。
    let data = json!({ "url": "file:///tmp/x.bin", "file": "x.bin", "base64": "" });
    assert_eq!(
        parse_platform_file_sources(&data),
        vec![PlatformFileSource::LocalPath("/tmp/x.bin".into())]
    );
    assert!(parse_platform_file_sources(&json!({})).is_empty());
    assert_eq!(
        platform_file_byte_limit("clip.MP4"),
        MAX_INBOUND_VIDEO_BYTES
    );
    assert_eq!(
        platform_file_byte_limit("notes.txt"),
        MAX_INBOUND_FILE_BYTES
    );
}

/// 同机部署时直接从桥的缓存拷文件,超限即拒且不留残片。
#[tokio::test]
async fn copies_local_platform_files_with_a_cap() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("source.mp4");
    tokio::fs::write(&source, b"0123456789").await.unwrap();
    let cache = tempfile::tempdir().unwrap();
    let copied = copy_platform_file_capped(&source, cache.path(), "clip.mp4", 10)
        .await
        .unwrap();
    assert_eq!(tokio::fs::read(&copied).await.unwrap(), b"0123456789");
    assert!(
        copy_platform_file_capped(&source, cache.path(), "clip.mp4", 9)
            .await
            .is_err()
    );
    let (_, count) = scan_platform_file_storage(cache.path(), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(count, 1, "超限的拷贝不能留残片");
}

#[test]
fn sanitizes_file_names() {
    assert_eq!(sanitize_file_name("../../etc/passwd"), "passwd");
    assert_eq!(sanitize_file_name("C:\\evil\\x.exe"), "x.exe");
    assert_eq!(sanitize_file_name(".."), "file");
    assert_eq!(sanitize_file_name("  "), "file");
    assert_eq!(sanitize_file_name("报告 v2.pdf"), "报告 v2.pdf");
}

/// 同名并发落盘不能互相覆盖，**而且返回路径时内容必须已经可读**。
///
/// 后半句曾经不成立：`save_platform_file` 少了一次 flush，`write_all` 只是把
/// 真正的写扔进 tokio 的阻塞线程池就返回 Ok（见该函数的注释）。线程池繁忙时
/// 这里读到的就是空文件——本用例因此长期偶发红（`left: [[], ...]`），一度被
/// 当成「并发时序 flake」记下来，其实是真的会丢数据。
///
/// 注意这条断言**依赖时序**：线程池空闲时不 flush 也能过。它抓得住回归，但
/// 不是每次都抓得住。
#[tokio::test]
async fn concurrent_inbound_files_with_the_same_name_do_not_overwrite() {
    let temp = tempfile::tempdir().unwrap();
    let first = save_platform_file(temp.path(), "report.txt", b"first");
    let second = save_platform_file(temp.path(), "report.txt", b"second");
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();

    assert_ne!(first, second);
    let mut contents = vec![
        tokio::fs::read(first).await.unwrap(),
        tokio::fs::read(second).await.unwrap(),
    ];
    contents.sort();
    assert_eq!(contents, vec![b"first".to_vec(), b"second".to_vec()]);
}

#[tokio::test]
async fn inbound_file_store_enforces_a_total_capacity() {
    let temp = tempfile::tempdir().unwrap();
    save_platform_file(temp.path(), "existing.bin", b"12345678")
        .await
        .unwrap();

    assert!(
        ensure_platform_file_capacity(temp.path(), 2, 10, 10, Duration::from_secs(60),)
            .await
            .is_ok()
    );
    assert!(
        ensure_platform_file_capacity(temp.path(), 3, 10, 10, Duration::from_secs(60),)
            .await
            .is_err()
    );
}

/// `save_platform_file` 返回路径时，文件内容必须已经完整可读。
///
/// 这不是「快一点慢一点」的问题：入站文件的路径会直接交给模型去读，写没落地
/// 就等于丢数据。而 `tokio::fs::File::write_all` 只把数据拷进内部缓冲、把真正
/// 的写扔给阻塞线程池就返回 Ok，drop 不等它完成。
///
/// 用**并发压满线程池**来确定性复现，而不是靠运气：漏 flush 时 400 次里有
/// 281 次读到不完整的文件；补上 flush 后 0 次。旁边那条
/// `concurrent_inbound_files_...` 也能抓到同一个 bug，但它只跑两个文件，得看
/// 线程池当时忙不忙——长期表现为「偶发红」，一度被误记成并发时序问题。
#[tokio::test(flavor = "multi_thread")]
async fn inbound_files_are_complete_when_the_path_is_handed_out() {
    const PAYLOAD_BYTES: usize = 64 * 1024;
    const CONCURRENCY: usize = 400;
    let temp = tempfile::tempdir().unwrap();
    let payload = vec![b'x'; PAYLOAD_BYTES];
    let mut handles = Vec::new();
    for index in 0..CONCURRENCY {
        let dir = temp.path().to_path_buf();
        let payload = payload.clone();
        handles.push(tokio::spawn(async move {
            let path = save_platform_file(&dir, &format!("f{index}.bin"), &payload)
                .await
                .unwrap();
            tokio::fs::metadata(&path).await.unwrap().len()
        }));
    }
    let mut incomplete = 0usize;
    for handle in handles {
        if handle.await.unwrap() != PAYLOAD_BYTES as u64 {
            incomplete += 1;
        }
    }
    assert_eq!(
        incomplete, 0,
        "{incomplete}/{CONCURRENCY} 个文件在返回路径时还没写完"
    );
}
