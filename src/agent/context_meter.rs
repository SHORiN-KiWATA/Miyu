//! 上下文表：优先用供应商报的真实占用，拿不到才退回本地估算。
//!
//! 本地估算走 o200k BPE，中文按 2 字/token 退化——不同供应商的分词器差得远，
//! 一换池子触发线就系统性偏。真实计数只在回合结束时拿得到，所以「锚点」记在
//! 回合行上，下一次问上下文占用时直接读它。

use crate::agent::*;

/// 一个已完成回合结束时的上下文占用：它最后一次请求的 prompt + completion。
/// `result.usage` 是整轮累计（多轮工具时是各请求之和），拿它当上下文占用会
/// 把同一段前缀数很多遍——这里只认 `last_request_usage`。
pub(in crate::agent) fn context_end_tokens(result: &ChatResult) -> Option<u64> {
    if result.usage_estimated {
        return None;
    }
    let usage = result.last_request_usage.as_ref()?;
    let total = usage.prompt_tokens.saturating_add(usage.completion_tokens);
    (total > 0).then_some(total)
}

impl Agent {
    /// 本地 o200k 估算：渲染整份请求再数 token（v3 之前的唯一口径，现为兜底）。
    pub fn context_tokens_estimate(&self) -> Result<u64> {
        let (messages, _) = self.chat_messages("", "")?;
        let mut tokens = overflow::estimate_messages_tokens(&messages) as u64;
        if self.tools_enabled {
            tokens = tokens.saturating_add(self.tool_definition_tokens() as u64);
        }
        Ok(tokens)
    }

    /// 最新一条已完成回合的供应商实测占用。刚压缩完（尾巴是摘要行）、回合被
    /// 打断、用量是估算的，都拿不到锚点，退回估算直到下一轮跑完。
    ///
    /// 刻意不校验供应商是否与当前池一致：池按请求轮换，换了端点也照样是一份
    /// 真实计数，比 o200k 硬数中文更接近真值。
    fn context_anchor_tokens(&self) -> Result<Option<u64>> {
        Ok(self
            .state
            .load_context_anchor()?
            .map(|anchor| anchor.tokens))
    }

    /// 触发线与 footer 用的上下文占用。
    ///
    /// 锚点是「上一次请求结束时」的占用，下一次请求 ≈ 锚点 + 新用户消息 +
    /// 瞬态尾巴；而估算路径本来就是拿空输入渲染的，两者口径一致，所以有锚点
    /// 时直接返回锚点，不做锚点后的增量估算。completion 里含推理 token（多数
    /// 供应商不回放推理），锚点因此略偏高——偏保守的方向，可接受。
    pub fn effective_context_tokens(&self) -> Result<u64> {
        let estimate = self.context_tokens_estimate()?;
        match self.context_anchor_tokens()? {
            Some(anchor) => {
                // 量尺：测具按 "context meter" 抓行算估算与真值的相对误差。
                tracing::info!(estimate, anchor, "context meter");
                Ok(anchor)
            }
            None => Ok(estimate),
        }
    }
}
