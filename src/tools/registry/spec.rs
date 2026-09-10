//! 工具规格与进度事件。
//!
//! `ToolSpec` 是一个工具的全部静态信息：名字、参数 schema、权限、是否懒加载。
//! 注册表只是这些规格的容器。
//!
//! `ToolProgress` 让长工具能往回报进度。它是**单向**的——工具不能通过它拿到
//! 用户输入，那条路走 question 机制。

use crate::tools::registry::*;
use tokio::sync::mpsc;

pub type ToolFuture = Pin<Box<dyn Future<Output = Result<String>> + Send>>;

pub type ToolHandler = Arc<dyn Fn(Value, ToolProgress) -> ToolFuture + Send + Sync>;

/// 单调执行守卫:返回 Some(理由) 即拒绝本次调用,返回 None 放行。
/// 只拒不放——任何 guard 拒绝后,后续 guard 与调用方都无法翻案
/// (dsh ToolGuard 同款语义)。拒绝以普通 tool error 回给模型,轮次存活。
pub type ToolGuard = Arc<dyn Fn(&ToolSpec, &Value, &GuardCtx) -> Option<String> + Send + Sync>;

/// 守卫可见的回合上下文。registry 自身无回合概念,由调用方(agent 主循环、
/// 未来的工具桥)按次构造;拿不到上下文的路径(subagent 的 call)用默认值,
/// 仅参数级守卫生效。
#[derive(Default)]
pub struct GuardCtx<'a> {
    /// 本回合此前已请求过的工具名(含本次,按主循环 push 时序)。
    pub used_tools: &'a [String],
}

#[derive(Debug)]
pub enum ToolProgressEvent {
    Message(String),
    PrepareForExternalOutput {
        ready: oneshot::Sender<bool>,
    },
    Image {
        path: PathBuf,
        alt: String,
        /// 模型显式要的终端渲染尺寸。百分比默认值不走这里——那个只能在
        /// 终端那一侧算。
        size: Option<String>,
    },
    Artifact {
        path: PathBuf,
        title: String,
    },
    CommandOutput {
        stream: CommandOutputStream,
        chunk: Vec<u8>,
    },
}

#[derive(Clone, Default)]
pub struct ToolProgress {
    pub(crate) sender: Option<mpsc::UnboundedSender<ToolProgressEvent>>,
}

impl ToolProgress {
    pub fn new(sender: mpsc::UnboundedSender<ToolProgressEvent>) -> Self {
        Self {
            sender: Some(sender),
        }
    }

    pub fn report(&self, message: impl Into<String>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ToolProgressEvent::Message(message.into()));
        }
    }

    pub fn report_command_output(&self, stream: CommandOutputStream, chunk: Vec<u8>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ToolProgressEvent::CommandOutput { stream, chunk });
        }
    }

    pub fn report_image(&self, path: impl Into<PathBuf>, alt: impl Into<String>) {
        self.report_sized_image(path, alt, None)
    }

    /// 带上模型显式要的尺寸；`None` 表示由终端按配置百分比自己定。
    pub fn report_sized_image(
        &self,
        path: impl Into<PathBuf>,
        alt: impl Into<String>,
        size: Option<String>,
    ) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ToolProgressEvent::Image {
                path: path.into(),
                alt: alt.into(),
                size,
            });
        }
    }

    pub fn report_artifact(&self, path: impl Into<PathBuf>, title: impl Into<String>) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(ToolProgressEvent::Artifact {
                path: path.into(),
                title: title.into(),
            });
        }
    }

    pub async fn prepare_for_external_output(&self) -> bool {
        let Some(sender) = &self.sender else {
            return true;
        };
        let (ready, receiver) = oneshot::channel();
        if sender
            .send(ToolProgressEvent::PrepareForExternalOutput { ready })
            .is_ok()
        {
            return receiver.await.unwrap_or(false);
        }
        false
    }
}

#[cfg(test)]
mod progress_tests {
    use crate::tools::*;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn external_output_waits_for_renderer_acknowledgement() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let progress = ToolProgress::new(sender);
        let prepare = progress.prepare_for_external_output();
        tokio::pin!(prepare);

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut prepare)
                .await
                .is_err()
        );

        let ToolProgressEvent::PrepareForExternalOutput { ready } = receiver.recv().await.unwrap()
        else {
            panic!("expected external output preparation event");
        };
        ready.send(true).unwrap();
        assert!(prepare.await);
    }
}

#[derive(Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub permission: ToolPermission,
    pub display_name: Option<String>,
    pub always_loaded: bool,
    pub is_script: bool,
    pub load_policy: LoadPolicy,
    pub groups: Vec<String>,
    /// 按工具超时（秒）。None=吃 registry 默认兜底；Some(0)=豁免。
    pub timeout_seconds: Option<u64>,
    /// 附在 stub 上的一行调用示例，例如 `{"query":"艾尔登法环"}`。
    ///
    /// stub 的参数壳是空的 `{"type":"object"}`，模型没取契约就调用时只能猜
    /// 字段名。给一个示例比让它猜便宜——也比事后用 `coerce_declared_shapes`
    /// 之类的补丁去修猜错的形状可靠。
    ///
    /// 判据是「猜错概率高，但契约又短到能塞进一行」，不是「形状简单」：单参数
    /// 工具本来就不容易猜错，加了也没收益；而 action 分发型（十来个参数、
    /// 按 action 条件必填）一行装不下，给半个示例比不给更危险——模型会以为
    /// 那就是全部形状。那类工具就该老实走 `load_tools`。
    pub stub_example: Option<String>,
    /// 能否暴露给不可信的场所(QQ 群这类 External 入口)。默认 Owner:
    /// 只有属主类入口看得见。注册位置曾是唯一的权限边界(restricted 注册表
    /// 硬编码白名单);这个位让脚本/插件自己声明,场所层按它过滤。
    pub trust: ToolTrust,
    /// 指路句:`(必须同时在场的工具, 追加到本工具描述末尾的句子)`。被指的
    /// 工具没注册就一个字都不加(见 cross_hints)。
    pub cross_hints: Vec<(String, String)>,
    /// 本回合必须先调用过这些工具之一才放行(跨工具闸,由 guard 层执行)。
    pub requires_prior: Vec<String>,
    pub(crate) handler: ToolHandler,
}

/// 工具对场所信任等级的要求。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolTrust {
    /// 只给属主类入口(终端、本机 WebUI、语音)。
    #[default]
    Owner,
    /// 也给不可信入口(QQ 群、远端 WebUI 成员)。
    External,
}

impl ToolTrust {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "owner" | "private" | "local" | "属主" => Some(Self::Owner),
            "external" | "platform" | "public" | "everyone" | "外部" | "公开" => {
                Some(Self::External)
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolPermission {
    ReadOnly,
    Presentation,
    Writes,
}

impl ToolPermission {
    pub fn parse(value: &str) -> Option<Self> {
        match value
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str()
        {
            "read_only" | "readonly" | "read" | "只读" => Some(Self::ReadOnly),
            "presentation" | "present" | "展示" => Some(Self::Presentation),
            "writes" | "write" | "读写" | "写" => Some(Self::Writes),
            _ => None,
        }
    }
}

impl ToolSpec {
    pub fn new<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            permission: ToolPermission::ReadOnly,
            display_name: None,
            always_loaded: true,
            is_script: false,
            load_policy: LoadPolicy::Summary,
            groups: Vec::new(),
            timeout_seconds: None,
            stub_example: None,
            trust: ToolTrust::Owner,
            cross_hints: Vec::new(),
            requires_prior: Vec::new(),
            handler: Arc::new(move |args, _progress| Box::pin(handler(args))),
        }
    }

    pub fn new_with_progress<F, Fut>(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        handler: F,
    ) -> Self
    where
        F: Fn(Value, ToolProgress) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<String>> + Send + 'static,
    {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            permission: ToolPermission::ReadOnly,
            display_name: None,
            always_loaded: true,
            is_script: false,
            load_policy: LoadPolicy::Summary,
            groups: Vec::new(),
            timeout_seconds: None,
            stub_example: None,
            trust: ToolTrust::Owner,
            cross_hints: Vec::new(),
            requires_prior: Vec::new(),
            handler: Arc::new(move |args, progress| Box::pin(handler(args, progress))),
        }
    }

    pub fn writes(mut self) -> Self {
        self.permission = ToolPermission::Writes;
        self
    }

    pub fn presentation(mut self) -> Self {
        self.permission = ToolPermission::Presentation;
        self
    }

    pub fn with_display_name(mut self, display_name: impl Into<String>) -> Self {
        self.display_name = Some(display_name.into());
        self
    }

    pub fn with_always_loaded(mut self, always_loaded: bool) -> Self {
        self.always_loaded = always_loaded;
        self
    }

    pub fn with_stub_example(mut self, example: impl Into<String>) -> Self {
        self.stub_example = Some(example.into());
        self
    }

    pub fn with_load_policy(mut self, load_policy: LoadPolicy) -> Self {
        self.load_policy = load_policy;
        self
    }

    pub fn with_groups(mut self, groups: Vec<String>) -> Self {
        self.groups = groups
            .into_iter()
            .map(|group| group.trim().to_string())
            .filter(|group| !group.is_empty())
            .collect();
        self
    }

    pub fn script(mut self) -> Self {
        self.is_script = true;
        self
    }

    pub fn with_trust(mut self, trust: ToolTrust) -> Self {
        self.trust = trust;
        self
    }

    pub fn with_permission(mut self, permission: ToolPermission) -> Self {
        self.permission = permission;
        self
    }

    pub fn with_cross_hints(mut self, hints: Vec<(String, String)>) -> Self {
        self.cross_hints = hints
            .into_iter()
            .filter(|(tool, sentence)| !tool.trim().is_empty() && !sentence.trim().is_empty())
            .collect();
        self
    }

    pub fn with_requires_prior(mut self, tools: Vec<String>) -> Self {
        self.requires_prior = tools
            .into_iter()
            .map(|tool| tool.trim().to_string())
            .filter(|tool| !tool.is_empty())
            .collect();
        self
    }

    pub fn with_timeout_seconds(mut self, secs: u64) -> Self {
        self.timeout_seconds = Some(secs);
        self
    }

    pub fn apply_built_in_description(mut self) -> Self {
        if let Some(desc) = crate::tools::tool_descriptions::get(&self.name) {
            // load_skill owns a dynamic catalog description, but still uses
            // the same loading policy, groups, schema, and display metadata
            // as every other built-in tool.
            if self.name != "load_skill" {
                self.description = desc.description.clone();
            }
            self.parameters = desc.parameters.clone();
            self.display_name = Some(desc.display_name.clone());
            self.always_loaded = desc.always_loaded;
            self.load_policy = desc.load_policy;
            self.groups = desc.groups.clone();
            if desc.timeout_seconds.is_some() {
                self.timeout_seconds = desc.timeout_seconds;
            }
        }
        self
    }

    pub fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            kind: "function",
            function: FunctionDefinition {
                name: self.name.clone(),
                description: self.description.clone(),
                parameters: self.parameters.clone(),
            },
        }
    }

    pub(crate) async fn call(&self, args: Value, progress: ToolProgress) -> Result<String> {
        (self.handler)(args, progress).await
    }

    pub(crate) fn call_future(&self, args: Value, progress: ToolProgress) -> ToolFuture {
        (self.handler)(args, progress)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnregisteredScript {
    pub name: String,
    pub path: String,
}
