//! ONNX Runtime 推理，只在 embedding worker 子进程里跑。
//!
//! 运行库是动态加载的（`ort` 的 `load-dynamic`）：构建期不下载任何东西，运行期
//! 找不到 `libonnxruntime` 就只是"本地语义不可用"。必须走 `ort::init_from`
//! 显式给路径——`ort` 的默认路径找不到库会直接 panic。
//!
//! 单线程、逐条推理：09-05 实测 batch 32 会让 ORT 内存池冲到 2 GB 且不归还，
//! 逐条反而最快（12 ms/条）且峰值只有 129 MB。

use super::manifest::{LocalModel, Pooling};
use anyhow::{anyhow, bail, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use tokenizers::Tokenizer;

pub(crate) const RUNTIME_LIB_ENV: &str = "MIYU_ONNXRUNTIME_LIB";

#[cfg(target_os = "macos")]
const RUNTIME_LIB_FILE: &str = "libonnxruntime.dylib";
#[cfg(target_os = "windows")]
const RUNTIME_LIB_FILE: &str = "onnxruntime.dll";
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
const RUNTIME_LIB_FILE: &str = "libonnxruntime.so";

pub(crate) fn candidate_runtime_libs() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for env in [RUNTIME_LIB_ENV, "ORT_DYLIB_PATH"] {
        if let Some(path) = std::env::var_os(env) {
            if !path.is_empty() {
                candidates.push(PathBuf::from(path));
            }
        }
    }
    if let Some(home) = crate::paths::miyu_home_dir() {
        candidates.push(home.join("lib").join(RUNTIME_LIB_FILE));
    }
    for dir in [
        "/usr/lib",
        "/usr/lib64",
        "/usr/local/lib",
        "/usr/lib/x86_64-linux-gnu",
        "/usr/lib/aarch64-linux-gnu",
        "/opt/homebrew/lib",
        "/usr/local/opt/onnxruntime/lib",
    ] {
        candidates.push(Path::new(dir).join(RUNTIME_LIB_FILE));
    }
    candidates
}

pub(crate) fn find_runtime_lib() -> Option<PathBuf> {
    candidate_runtime_libs()
        .into_iter()
        .find(|path| path.is_file())
}

pub(crate) struct LocalEncoder {
    session: Session,
    tokenizer: Tokenizer,
    model: LocalModel,
    input_names: Vec<String>,
}

impl LocalEncoder {
    pub(crate) fn load(model: &LocalModel, runtime_lib: &Path) -> Result<Self> {
        let environment = ort::init_from(runtime_lib).map_err(|error| {
            anyhow!(
                "loading ONNX Runtime from {}: {error}",
                runtime_lib.display()
            )
        })?;
        environment.with_name("miyu").commit();
        let tokenizer_path = model.tokenizer_path();
        let mut tokenizer = Tokenizer::from_file(&tokenizer_path)
            .map_err(|error| anyhow!("loading {}: {error}", tokenizer_path.display()))?;
        tokenizer
            .with_truncation(Some(tokenizers::TruncationParams {
                max_length: model.manifest.max_length.max(8),
                ..Default::default()
            }))
            .map_err(|error| anyhow!("configuring tokenizer truncation: {error}"))?;
        tokenizer.with_padding(None);
        let model_path = model.model_path();
        // Builder errors carry the builder back (not `Send`), so they cannot
        // ride `?` into `anyhow`; render them to text here.
        fn builder_error<T>(error: ort::Error<T>) -> anyhow::Error {
            anyhow!("configuring the ONNX session: {error}")
        }
        let session = Session::builder()
            .map_err(builder_error)?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(builder_error)?
            .with_intra_threads(1)
            .map_err(builder_error)?
            .with_inter_threads(1)
            .map_err(builder_error)?
            .commit_from_file(&model_path)
            .map_err(builder_error)
            .with_context(|| format!("loading {}", model_path.display()))?;
        let input_names = session
            .inputs()
            .iter()
            .map(|input| input.name().to_string())
            .collect::<Vec<_>>();
        if !input_names.iter().any(|name| name == "input_ids") {
            bail!(
                "{} has no `input_ids` input (inputs: {})",
                model_path.display(),
                input_names.join(", ")
            );
        }
        Ok(Self {
            session,
            tokenizer,
            model: model.clone(),
            input_names,
        })
    }

    pub(crate) fn dims(&self) -> usize {
        self.model.manifest.dims
    }

    pub(crate) fn encode(&mut self, text: &str) -> Result<Vec<f32>> {
        let text = if text.trim().is_empty() { " " } else { text };
        let encoding = self
            .tokenizer
            .encode(text, true)
            .map_err(|error| anyhow!("tokenizing: {error}"))?;
        let ids = encoding
            .get_ids()
            .iter()
            .map(|&id| id as i64)
            .collect::<Vec<_>>();
        let mask = encoding
            .get_attention_mask()
            .iter()
            .map(|&value| value as i64)
            .collect::<Vec<_>>();
        let type_ids = encoding
            .get_type_ids()
            .iter()
            .map(|&value| value as i64)
            .collect::<Vec<_>>();
        let length = ids.len();
        if length == 0 {
            bail!("tokenizer produced no tokens");
        }
        let mut inputs = Vec::with_capacity(self.input_names.len());
        for name in &self.input_names {
            let data = match name.as_str() {
                "input_ids" => ids.clone(),
                "attention_mask" => mask.clone(),
                "token_type_ids" => type_ids.clone(),
                other => bail!("embedding model has an unsupported input `{other}`"),
            };
            let tensor = Tensor::from_array(([1usize, length], data))?;
            inputs.push((
                std::borrow::Cow::<str>::Owned(name.clone()),
                ort::session::SessionInputValue::from(tensor),
            ));
        }
        let outputs = self.session.run(inputs)?;
        let (shape, data) = outputs[0].try_extract_tensor::<f32>()?;
        let dims_i64 = shape.iter().copied().collect::<Vec<i64>>();
        let (tokens, hidden) = match dims_i64.as_slice() {
            [_, tokens, hidden] => (*tokens as usize, *hidden as usize),
            [_, hidden] => (1, *hidden as usize),
            other => bail!("unexpected embedding output shape {other:?}"),
        };
        if hidden == 0 || data.len() < tokens * hidden {
            bail!("embedding output is truncated");
        }
        let mut vector = match self.model.manifest.pooling {
            Pooling::Cls => data[..hidden].to_vec(),
            Pooling::Mean => {
                let mut sum = vec![0.0_f32; hidden];
                let mut count = 0.0_f32;
                for token in 0..tokens {
                    if mask.get(token).copied().unwrap_or(1) == 0 {
                        continue;
                    }
                    let row = &data[token * hidden..(token + 1) * hidden];
                    for (acc, value) in sum.iter_mut().zip(row) {
                        *acc += value;
                    }
                    count += 1.0;
                }
                if count > 0.0 {
                    for value in &mut sum {
                        *value /= count;
                    }
                }
                sum
            }
        };
        if self.model.manifest.normalize {
            let norm = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 0.0 {
                for value in &mut vector {
                    *value /= norm;
                }
            }
        }
        if vector.len() != self.model.manifest.dims {
            bail!(
                "model produced {} dims but manifest says {}",
                vector.len(),
                self.model.manifest.dims
            );
        }
        Ok(vector)
    }
}
