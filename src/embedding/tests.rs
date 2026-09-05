use super::manifest::DEFAULT_LOCAL_MODEL;
use super::*;

#[test]
fn blob_roundtrip_is_exact() {
    let vector = vec![0.0_f32, -1.5, 3.25, f32::MIN_POSITIVE, 1e10];
    let blob = vector_to_blob(&vector);
    assert_eq!(blob.len(), vector.len() * 4);
    assert_eq!(vector_from_blob(&blob).unwrap(), vector);
    assert!(vector_from_blob(&[]).is_none());
    assert!(vector_from_blob(&[1, 2, 3]).is_none());
}

#[test]
fn cosine_of_orthogonal_and_identical_vectors() {
    assert!((cosine(&[1.0, 0.0], &[1.0, 0.0]) - 1.0).abs() < 1e-6);
    assert!(cosine(&[1.0, 0.0], &[0.0, 1.0]).abs() < 1e-6);
    assert_eq!(cosine(&[1.0], &[1.0, 2.0]), 0.0);
    assert_eq!(cosine(&[0.0, 0.0], &[1.0, 1.0]), 0.0);
}

#[test]
fn rrf_prefers_items_ranked_well_by_both_lists() {
    let keyword = vec!["a", "b", "c"];
    let semantic = vec!["b", "d", "c"];
    let fused = rrf_fuse(&[keyword, semantic], RRF_K);
    let order: Vec<&str> = fused.iter().map(|(key, _)| *key).collect();
    // b: 1/62 + 1/61 beats c (1/63 + 1/63), which beats a (1/61 alone).
    assert_eq!(order[0], "b");
    assert_eq!(order[1], "c");
    assert_eq!(order[2], "a");
    assert!(order.contains(&"d"));
    assert_eq!(order.len(), 4);
}

#[test]
fn rrf_ties_keep_the_first_ranking_order() {
    let fused = rrf_fuse(&[vec!["x", "y"], vec!["y", "x"]], RRF_K);
    let order: Vec<&str> = fused.iter().map(|(key, _)| *key).collect();
    assert_eq!(order, vec!["x", "y"]);
}

#[test]
fn manifest_defaults_fill_in_optional_fields() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("manifest.json"), r#"{"id":"toy","dims":4}"#).unwrap();
    std::fs::write(dir.path().join("model.onnx"), b"x").unwrap();
    std::fs::write(dir.path().join("tokenizer.json"), b"{}").unwrap();
    let model = manifest::load_model_dir(dir.path()).unwrap();
    assert_eq!(model.manifest.pooling, manifest::Pooling::Cls);
    assert!(model.manifest.normalize);
    assert_eq!(model.manifest.max_length, 512);
    assert_eq!(model.model_id(), "local:toy");
    assert_eq!(model.model_path(), dir.path().join("model.onnx"));
}

#[test]
fn manifest_rejects_missing_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("manifest.json"), r#"{"id":"toy","dims":4}"#).unwrap();
    let error = manifest::load_model_dir(dir.path()).unwrap_err();
    assert!(error.to_string().contains("missing"), "{error}");
}

#[test]
fn resolving_a_path_loads_that_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        r#"{"id":"toy","dims":4,"pooling":"mean","min_score":0.5}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("model.onnx"), b"x").unwrap();
    std::fs::write(dir.path().join("tokenizer.json"), b"{}").unwrap();
    let model = resolve_local_model(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(model.manifest.pooling, manifest::Pooling::Mean);
    assert!((model.manifest.min_score - 0.5).abs() < f32::EPSILON);
    assert!(resolve_local_model("definitely-not-installed-model").is_err());
}

/// The bundled asset must stay loadable from the source tree: the dev lookup
/// chain is what `cargo run` and the tests use.
#[test]
fn bundled_default_model_resolves_in_the_source_tree() {
    let model = resolve_local_model(DEFAULT_LOCAL_MODEL).unwrap();
    assert_eq!(model.manifest.id, DEFAULT_LOCAL_MODEL);
    assert_eq!(model.manifest.dims, 512);
    assert!(model.model_path().is_file());
}

/// Real inference in-process (the `cfg(test)` form of `embed_via_worker`).
/// Needs the ONNX Runtime library on this machine; skips (loudly) otherwise
/// so packaging builds without it still pass.
#[tokio::test]
async fn local_model_embeds_real_text_when_the_runtime_is_installed() {
    if runtime_library().is_err() {
        eprintln!("skipping: ONNX Runtime library not installed");
        return;
    }
    let model = resolve_local_model(DEFAULT_LOCAL_MODEL).unwrap();
    let vectors = worker::embed_via_worker(
        &model,
        Duration::from_secs(5),
        &[
            "今天天气不错".to_string(),
            "Arch Linux 更新后无法启动".to_string(),
        ],
    )
    .await
    .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[0].len(), 512);
    let norm = vectors[0].iter().map(|v| v * v).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "normalized, got {norm}");
    let same = worker::embed_via_worker(
        &model,
        Duration::from_secs(5),
        &["今天天气很好".to_string()],
    )
    .await
    .unwrap();
    let near = cosine(&vectors[0], &same[0]);
    let far = cosine(&vectors[1], &same[0]);
    assert!(near > far, "paraphrase {near} should beat unrelated {far}");
}

/// The worker protocol end to end over an in-memory pipe: handshake, a real
/// request, an oversized request being rejected without killing the worker,
/// and a clean exit when the client hangs up.
#[tokio::test]
async fn worker_protocol_roundtrips_over_a_pipe() {
    let Ok(runtime_lib) = runtime_library() else {
        eprintln!("skipping: ONNX Runtime library not installed");
        return;
    };
    let model = resolve_local_model(DEFAULT_LOCAL_MODEL).unwrap();
    let encoder = local::LocalEncoder::load(&model, &runtime_lib).unwrap();
    let (mut client_in, mut worker_out) = tokio::io::duplex(1 << 20);
    let (mut worker_in, mut client_out) = tokio::io::duplex(1 << 20);
    let server = tokio::spawn(async move {
        worker::serve(
            &mut worker_in,
            &mut worker_out,
            Duration::from_secs(30),
            Ok(encoder),
        )
        .await
    });
    // Handshake carries the dims.
    let (dims, empty) = worker::read_response(&mut client_in).await.unwrap();
    assert_eq!(dims, 512);
    assert!(empty.is_empty());
    let vectors = worker::exchange_io(
        &mut client_out,
        &mut client_in,
        &["你好".to_string(), "world".to_string()],
    )
    .await
    .unwrap();
    assert_eq!(vectors.len(), 2);
    assert_eq!(vectors[1].len(), 512);
    // Too many texts: rejected, but the worker stays up for the next call.
    let too_many: Vec<String> = (0..300).map(|i| i.to_string()).collect();
    match worker::exchange_io(&mut client_out, &mut client_in, &too_many).await {
        Err(worker::ExchangeError::Rejected(message)) => {
            assert!(message.contains("too many"), "{message}")
        }
        other => panic!("expected rejection, got {other:?}"),
    }
    let again = worker::exchange_io(&mut client_out, &mut client_in, &["再来".to_string()])
        .await
        .unwrap();
    assert_eq!(again.len(), 1);
    drop(client_out);
    server.await.unwrap().unwrap();
}

/// A load failure is reported through the handshake, not by dying silently.
#[tokio::test]
async fn worker_reports_a_load_failure_in_the_handshake() {
    let (mut client_in, mut worker_out) = tokio::io::duplex(1 << 16);
    let (mut worker_in, _client_out) = tokio::io::duplex(1 << 16);
    let server = tokio::spawn(async move {
        worker::serve(
            &mut worker_in,
            &mut worker_out,
            Duration::from_secs(5),
            Err(anyhow::anyhow!("libonnxruntime.so is missing")),
        )
        .await
    });
    match worker::read_response(&mut client_in).await {
        Err(worker::ExchangeError::Rejected(message)) => {
            assert!(message.contains("libonnxruntime"), "{message}")
        }
        other => panic!("expected the load error, got {other:?}"),
    }
    server.await.unwrap().unwrap();
}
