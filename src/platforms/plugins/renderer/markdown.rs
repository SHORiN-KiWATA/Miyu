//! Markdown → 块结构。
//!
//! 只收集结构，不管排版。`validate_markdown` 在这一步就挡掉超长输入
//! （`MAX_INPUT_CHARS`）——后面每一步的开销都跟输入量成正比甚至更差。

use crate::platforms::plugins::renderer::*;

pub(in crate::platforms::plugins::renderer) const MAX_INPUT_CHARS: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::platforms::plugins::renderer) enum BlockKind {
    Paragraph,
    Heading(u8),
    ListItem { depth: u8 },
    Quote,
    Code,
    Table,
    Rule,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::platforms::plugins::renderer) struct InlineStyle {
    pub(in crate::platforms::plugins::renderer) bold: bool,
    pub(in crate::platforms::plugins::renderer) italic: bool,
    pub(in crate::platforms::plugins::renderer) code: bool,
    pub(in crate::platforms::plugins::renderer) link: bool,
    pub(in crate::platforms::plugins::renderer) muted: bool,
}

#[derive(Clone, Debug)]
pub(in crate::platforms::plugins::renderer) struct RichSpan {
    pub(in crate::platforms::plugins::renderer) text: String,
    pub(in crate::platforms::plugins::renderer) style: InlineStyle,
}

#[derive(Clone, Debug)]
pub(in crate::platforms::plugins::renderer) struct Block {
    pub(in crate::platforms::plugins::renderer) kind: BlockKind,
    pub(in crate::platforms::plugins::renderer) spans: Vec<RichSpan>,
    pub(in crate::platforms::plugins::renderer) table: Option<TableBlock>,
    pub(in crate::platforms::plugins::renderer) task: Option<bool>,
}

impl Block {
    pub(in crate::platforms::plugins::renderer) fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            spans: Vec::new(),
            table: None,
            task: None,
        }
    }

    pub(in crate::platforms::plugins::renderer) fn push(&mut self, text: &str, style: InlineStyle) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.spans.last_mut().filter(|last| last.style == style) {
            last.text.push_str(text);
        } else {
            self.spans.push(RichSpan {
                text: text.to_string(),
                style,
            });
        }
    }

    pub(in crate::platforms::plugins::renderer) fn has_content(&self) -> bool {
        self.kind == BlockKind::Rule
            || self.spans.iter().any(|span| !span.text.is_empty())
            || self.table.as_ref().is_some_and(TableBlock::has_content)
            || self.task.is_some()
    }
}

#[derive(Clone, Debug)]
pub(in crate::platforms::plugins::renderer) struct TableBlock {
    pub(in crate::platforms::plugins::renderer) alignments: Vec<Alignment>,
    pub(in crate::platforms::plugins::renderer) header: Vec<Vec<RichSpan>>,
    pub(in crate::platforms::plugins::renderer) rows: Vec<Vec<Vec<RichSpan>>>,
}

impl TableBlock {
    pub(in crate::platforms::plugins::renderer) fn has_content(&self) -> bool {
        !self.header.is_empty() || !self.rows.is_empty()
    }
}

#[derive(Default)]
pub(in crate::platforms::plugins::renderer) struct TableBuilder {
    pub(in crate::platforms::plugins::renderer) alignments: Vec<Alignment>,
    pub(in crate::platforms::plugins::renderer) header: Vec<Vec<RichSpan>>,
    pub(in crate::platforms::plugins::renderer) rows: Vec<Vec<Vec<RichSpan>>>,
    pub(in crate::platforms::plugins::renderer) current_row: Vec<Vec<RichSpan>>,
    pub(in crate::platforms::plugins::renderer) current_cell: Vec<RichSpan>,
    pub(in crate::platforms::plugins::renderer) in_cell: bool,
}

impl TableBuilder {
    pub(in crate::platforms::plugins::renderer) fn push(&mut self, text: &str, style: InlineStyle) {
        if text.is_empty() || !self.in_cell {
            return;
        }
        if let Some(last) = self
            .current_cell
            .last_mut()
            .filter(|last| last.style == style)
        {
            last.text.push_str(text);
        } else {
            self.current_cell.push(RichSpan {
                text: text.to_string(),
                style,
            });
        }
    }

    pub(in crate::platforms::plugins::renderer) fn start_row(&mut self) {
        self.current_row.clear();
        self.current_cell.clear();
        self.in_cell = false;
    }

    pub(in crate::platforms::plugins::renderer) fn start_cell(&mut self) {
        self.current_cell.clear();
        self.in_cell = true;
    }

    pub(in crate::platforms::plugins::renderer) fn finish_cell(&mut self) {
        if self.in_cell {
            self.current_row
                .push(std::mem::take(&mut self.current_cell));
            self.in_cell = false;
        }
    }

    pub(in crate::platforms::plugins::renderer) fn finish_row(&mut self, header: bool) {
        self.finish_cell();
        let row = std::mem::take(&mut self.current_row);
        if row.is_empty() {
            return;
        }
        if header {
            self.header = row;
        } else {
            self.rows.push(row);
        }
    }

    pub(in crate::platforms::plugins::renderer) fn finish(mut self) -> TableBlock {
        self.finish_cell();
        TableBlock {
            alignments: self.alignments,
            header: self.header,
            rows: self.rows,
        }
    }
}

pub(in crate::platforms::plugins::renderer) struct ListState {
    pub(in crate::platforms::plugins::renderer) ordered: bool,
    pub(in crate::platforms::plugins::renderer) next: u64,
    pub(in crate::platforms::plugins::renderer) in_item: bool,
    pub(in crate::platforms::plugins::renderer) prefix_used: bool,
}

#[derive(Default)]
pub(in crate::platforms::plugins::renderer) struct MarkdownCollector {
    pub(in crate::platforms::plugins::renderer) blocks: Vec<Block>,
    pub(in crate::platforms::plugins::renderer) current: Option<Block>,
    pub(in crate::platforms::plugins::renderer) lists: Vec<ListState>,
    pub(in crate::platforms::plugins::renderer) quote_depth: usize,
    pub(in crate::platforms::plugins::renderer) heading: Option<u8>,
    pub(in crate::platforms::plugins::renderer) code_block: bool,
    pub(in crate::platforms::plugins::renderer) table: Option<TableBuilder>,
    pub(in crate::platforms::plugins::renderer) table_header: bool,
    pub(in crate::platforms::plugins::renderer) strong_depth: usize,
    pub(in crate::platforms::plugins::renderer) emphasis_depth: usize,
    pub(in crate::platforms::plugins::renderer) link_depth: usize,
    /// 未闭合链接的目标地址栈（可嵌套：图片可以套在链接里）。
    pub(in crate::platforms::plugins::renderer) link_urls: Vec<String>,
    /// 与上面一一对应的可见文字，用来判断「标题本身就是网址」。
    pub(in crate::platforms::plugins::renderer) link_texts: Vec<String>,
    pub(in crate::platforms::plugins::renderer) strike_depth: usize,
}

/// `[标题](链接)` 和 `![alt](图片地址)` 画成图之后地址会整个消失——图里点不了
/// 也复制不了，读者连它指向哪都无从知道。所以在可见文字后面补一段 ` (地址)`；
/// 可见文字为空（`![](url)` 这种）时整块只剩地址，那就单独把它放出来。
fn link_suffix(url: &str, shown: &str) -> Option<String> {
    let url = url.trim();
    let shown = shown.trim();
    if url.is_empty() || same_target(url, shown) {
        return None;
    }
    Some(if shown.is_empty() {
        format!("({url})")
    } else {
        format!(" ({url})")
    })
}

/// 自动链接（`<https://x>` 或 `[https://x](https://x)`）的标题本身就是网址，
/// 再补一遍只是把同一串东西写两次。
fn same_target(url: &str, shown: &str) -> bool {
    if url == shown {
        return true;
    }
    ["https://", "http://", "mailto:"].iter().any(|scheme| {
        url.strip_prefix(scheme)
            .is_some_and(|rest| rest.trim_end_matches('/') == shown.trim_end_matches('/'))
    })
}

impl MarkdownCollector {
    pub(in crate::platforms::plugins::renderer) fn collect(mut self, markdown: &str) -> Vec<Block> {
        let options =
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
        for event in Parser::new_ext(markdown, options) {
            self.event(event);
        }
        self.finish_current();
        self.blocks
    }

    pub(in crate::platforms::plugins::renderer) fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.push_text(&text, self.style()),
            Event::Code(text) => {
                let mut style = self.style();
                style.code = true;
                self.push_text(&text, style);
            }
            Event::InlineMath(text) | Event::DisplayMath(text) => {
                let mut style = self.style();
                style.code = true;
                self.push_text(&text, style);
            }
            Event::SoftBreak => {
                let separator = if self.code_block || self.table.is_some() {
                    "\n"
                } else {
                    " "
                };
                self.push_text(separator, self.style());
            }
            Event::HardBreak => self.push_text("\n", self.style()),
            Event::Rule => {
                self.finish_current();
                self.blocks.push(Block::new(BlockKind::Rule));
            }
            Event::TaskListMarker(done) => {
                self.mark_task(done);
            }
            Event::FootnoteReference(label) => {
                self.push_text(&format!("[{label}]"), self.style());
            }
            Event::Html(text) | Event::InlineHtml(text) => {
                self.push_text(&text, self.style());
            }
        }
    }

    pub(in crate::platforms::plugins::renderer) fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.ensure_current(),
            Tag::Heading { level, .. } => {
                self.finish_current();
                let level = level as u8;
                self.heading = Some(level);
                self.current = Some(Block::new(BlockKind::Heading(level)));
            }
            Tag::BlockQuote(_) => {
                self.finish_current();
                self.quote_depth = self.quote_depth.saturating_add(1);
            }
            Tag::CodeBlock(_) => {
                self.finish_current();
                self.code_block = true;
                self.current = Some(Block::new(BlockKind::Code));
            }
            Tag::List(start) => {
                self.finish_current();
                self.lists.push(ListState {
                    ordered: start.is_some(),
                    next: start.unwrap_or(1),
                    in_item: false,
                    prefix_used: false,
                });
            }
            Tag::Item => {
                self.finish_current();
                if let Some(list) = self.lists.last_mut() {
                    list.in_item = true;
                    list.prefix_used = false;
                }
                self.ensure_current();
            }
            Tag::Table(alignments) => {
                self.finish_current();
                self.table = Some(TableBuilder {
                    alignments,
                    ..TableBuilder::default()
                });
                self.table_header = false;
            }
            Tag::TableHead => {
                self.table_header = true;
                if let Some(table) = self.table.as_mut() {
                    table.start_row();
                }
            }
            Tag::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.start_row();
                }
            }
            Tag::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.start_cell();
                }
            }
            Tag::Strong => self.strong_depth = self.strong_depth.saturating_add(1),
            Tag::Emphasis => self.emphasis_depth = self.emphasis_depth.saturating_add(1),
            Tag::Strikethrough => self.strike_depth = self.strike_depth.saturating_add(1),
            // 图片和链接在这里是同一件事:图渲染器画不出图片,`![alt](url)`
            // 只剩 alt、`![](url)` 什么都不剩,读者更够不到那张图。
            Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                self.link_depth = self.link_depth.saturating_add(1);
                self.link_urls.push(dest_url.to_string());
                self.link_texts.push(String::new());
            }
            _ => {}
        }
    }

    pub(in crate::platforms::plugins::renderer) fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                if !self.code_block && self.table.is_none() && self.heading.is_none() {
                    self.finish_current();
                }
            }
            TagEnd::Heading(_) => {
                self.finish_current();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.finish_current();
                self.quote_depth = self.quote_depth.saturating_sub(1);
            }
            TagEnd::CodeBlock => {
                self.finish_current();
                self.code_block = false;
            }
            TagEnd::List(_) => {
                self.finish_current();
                self.lists.pop();
            }
            TagEnd::Item => {
                self.finish_current();
                if let Some(list) = self.lists.last_mut() {
                    list.in_item = false;
                }
            }
            TagEnd::Table => {
                if let Some(table) = self.table.take() {
                    let mut block = Block::new(BlockKind::Table);
                    block.table = Some(table.finish());
                    if block.has_content() {
                        self.blocks.push(block);
                    }
                }
                self.table_header = false;
            }
            TagEnd::TableHead => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_row(true);
                }
                self.table_header = false;
            }
            TagEnd::TableRow => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_row(self.table_header);
                }
            }
            TagEnd::TableCell => {
                if let Some(table) = self.table.as_mut() {
                    table.finish_cell();
                }
            }
            TagEnd::Strong => self.strong_depth = self.strong_depth.saturating_sub(1),
            TagEnd::Emphasis => self.emphasis_depth = self.emphasis_depth.saturating_sub(1),
            TagEnd::Strikethrough => self.strike_depth = self.strike_depth.saturating_sub(1),
            TagEnd::Link | TagEnd::Image => {
                self.link_depth = self.link_depth.saturating_sub(1);
                let url = self.link_urls.pop().unwrap_or_default();
                let text = self.link_texts.pop().unwrap_or_default();
                if let Some(suffix) = link_suffix(&url, &text) {
                    let mut style = self.style();
                    style.link = true;
                    self.push_text(&suffix, style);
                }
            }
            _ => {}
        }
    }

    pub(in crate::platforms::plugins::renderer) fn ensure_current(&mut self) {
        if self.current.is_some() {
            return;
        }
        if self.code_block {
            self.current = Some(Block::new(BlockKind::Code));
            return;
        }
        if self.table.is_some() {
            return;
        }
        if let Some(level) = self.heading {
            self.current = Some(Block::new(BlockKind::Heading(level)));
            return;
        }

        if let Some(index) = self.lists.iter().rposition(|list| list.in_item) {
            let depth = u8::try_from(index + 1).unwrap_or(u8::MAX);
            let list = &mut self.lists[index];
            let prefix = if list.prefix_used {
                "    ".to_string()
            } else if list.ordered {
                let number = list.next;
                list.next = list.next.saturating_add(1);
                list.prefix_used = true;
                format!("{number}. ")
            } else {
                list.prefix_used = true;
                "• ".to_string()
            };
            let mut block = Block::new(BlockKind::ListItem { depth });
            block.push(&prefix, InlineStyle::default());
            self.current = Some(block);
        } else if self.quote_depth > 0 {
            self.current = Some(Block::new(BlockKind::Quote));
        } else {
            self.current = Some(Block::new(BlockKind::Paragraph));
        }
    }

    pub(in crate::platforms::plugins::renderer) fn push_text(
        &mut self,
        text: &str,
        style: InlineStyle,
    ) {
        if let Some(shown) = self.link_texts.last_mut() {
            shown.push_str(text);
        }
        if let Some(table) = self.table.as_mut() {
            table.push(text, style);
            return;
        }
        self.ensure_current();
        if let Some(block) = self.current.as_mut() {
            block.push(text, style);
        }
    }

    pub(in crate::platforms::plugins::renderer) fn mark_task(&mut self, done: bool) {
        self.ensure_current();
        let Some(block) = self.current.as_mut() else {
            return;
        };
        if let Some(first) = block.spans.first_mut() {
            if let Some(rest) = first.text.strip_prefix("• ") {
                first.text = rest.to_string();
                if first.text.is_empty() {
                    block.spans.remove(0);
                }
            }
        }
        block.task = Some(done);
    }

    pub(in crate::platforms::plugins::renderer) fn style(&self) -> InlineStyle {
        InlineStyle {
            bold: self.strong_depth > 0 || self.table_header,
            italic: self.emphasis_depth > 0,
            code: self.code_block,
            link: self.link_depth > 0,
            muted: self.strike_depth > 0,
        }
    }

    pub(in crate::platforms::plugins::renderer) fn finish_current(&mut self) {
        let Some(block) = self.current.take() else {
            return;
        };
        if block.has_content() {
            self.blocks.push(block);
        }
    }
}

pub(in crate::platforms::plugins::renderer) fn collect_blocks(markdown: &str) -> Vec<Block> {
    MarkdownCollector::default().collect(markdown)
}

pub(in crate::platforms::plugins::renderer) fn validate_markdown(markdown: &str) -> Result<()> {
    let count = markdown.chars().take(MAX_INPUT_CHARS + 1).count();
    if count > MAX_INPUT_CHARS {
        bail!("Markdown image input exceeds the {MAX_INPUT_CHARS}-character limit");
    }
    Ok(())
}
