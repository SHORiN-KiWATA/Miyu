//! `miyu-voice`:语音前端进程的可执行入口。
//!
//! 与主程序 `miyu` 共用同一个 lib crate,但只有它链接 sherpa-onnx。
//! 默认形态是被 daemon 拉起的 worker;`test` / `devices` / `cue` 子命令
//! 供人工排查。

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "miyu-voice", about = "Miyu 语音前端(唤醒词 + 本地识别)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 不连 daemon,打开麦克风把唤醒/识别事件逐行打印。
    Test {
        /// 覆盖配置里的唤醒词(仅本次)。
        #[arg(long)]
        keyword: Option<String>,
        /// 覆盖配置里的麦克风设备名(仅本次)。
        #[arg(long)]
        device: Option<String>,
        /// 打印各阶段耗时。
        #[arg(long)]
        timings: bool,
    },
    /// 列出可选的输入源(`源名<TAB>描述`),源名写进配置的 microphone。
    Devices,
    /// 试听提示音:wake | heard | done | error。
    Cue {
        name: String,
        #[arg(long, default_value_t = 0.6)]
        volume: f32,
    },
}

fn main() {
    // glibc 默认按 8×CPU 数开 malloc arena;这个进程只有几个线程。
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    tracing_subscriber::fmt()
        .with_max_level(if std::env::var_os("MIYU_VOICE_DEBUG").is_some() {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .with_writer(std::io::stderr)
        .with_target(false)
        .init();
    let cli = Cli::parse();
    let outcome = match cli.command {
        None => miyu::voice::worker::run_worker(),
        Some(Command::Test {
            keyword,
            device,
            timings,
        }) => miyu::voice::worker::run_test(keyword, device, timings),
        Some(Command::Devices) => {
            // 一行一个:`源名<TAB>描述`,源名写进配置。
            for source in miyu::voice::mic::list_input_sources() {
                println!("{}\t{}", source.name, source.label);
            }
            Ok(())
        }
        Some(Command::Cue { name, volume }) => miyu::voice::worker::play_cue(&name, volume),
    };
    if let Err(error) = outcome {
        eprintln!("{}: {error:#}", miyu::error_label());
        std::process::exit(1);
    }
}
