//! 表格列宽分配。
//!
//! 长图的表格宽度是固定的（`COLUMN_WIDTH`），所以列宽必须**恰好**加起来等于
//! 它——`paint::draw_table_fragment` 靠 `cell.x + cell.width == COLUMN_WIDTH`
//! 判断哪一根是最右边的竖线。等分曾经满足这条，代价是 5 列表格里「排名」和
//! 「笔数」各占 192 px 装一个字符，而九位 QQ 号只剩 164 px 装不下，被逐字劈成
//! 两行。
//!
//! 这里做的是经典的两段式：能装下就按内容给，装不下就削最宽的那几列
//! （max-min 公平，不是按比例削），窄列不受牵连。

/// 按每列的自然文字宽度分配列宽，结果之和恒等于 `total`。
///
/// `natural[i]` 是第 i 列所有单元格里最宽的那行文字的像素宽度（不含 padding）。
/// `cell_padding` 是单元格单侧内边距，`min_content` 是希望给每列留的最小文字
/// 宽度——列太多时它会被自动压低，绝不会让总宽超出 `total`。
pub(in crate::platforms::plugins::renderer) fn plan_table_columns(
    natural: &[u32],
    total: u32,
    cell_padding: u32,
    min_content: u32,
) -> Vec<u32> {
    let count = natural.len();
    if count == 0 {
        return Vec::new();
    }
    let count_u32 = count as u32;
    let even = total / count_u32;
    // 列多到连 min_content 都摆不下时按等分退让：宁可回到旧行为，也不能让
    // 各列之和溢出画布。
    let min_outer = cell_padding
        .saturating_mul(2)
        .saturating_add(min_content)
        .min(even);

    let desired = natural
        .iter()
        .map(|width| {
            width
                .saturating_add(cell_padding.saturating_mul(2))
                .max(min_outer)
        })
        .collect::<Vec<_>>();
    let wanted = desired.iter().map(|width| u64::from(*width)).sum::<u64>();

    let mut widths = if wanted <= u64::from(total) {
        grow(&desired, natural, total, wanted)
    } else {
        shrink(&desired, total, min_outer)
    };
    settle(&mut widths, total);
    widths
}

/// 装得下：把富余按各列的自然宽度分给它们。文字多的列拿得多，只装一个字符的
/// 序号列不会白白撑成大空格。
fn grow(desired: &[u32], natural: &[u32], total: u32, wanted: u64) -> Vec<u32> {
    let surplus = u64::from(total).saturating_sub(wanted);
    let weight_sum = natural.iter().map(|width| u64::from(*width)).sum::<u64>();
    if surplus == 0 {
        return desired.to_vec();
    }
    // 整张表都是空单元格时没有权重可依，退回均分。
    if weight_sum == 0 {
        let share = surplus / desired.len() as u64;
        return desired
            .iter()
            .map(|width| width.saturating_add(share as u32))
            .collect();
    }
    desired
        .iter()
        .zip(natural)
        .map(|(width, weight)| {
            let share = surplus.saturating_mul(u64::from(*weight)) / weight_sum;
            width.saturating_add(share as u32)
        })
        .collect()
}

/// 装不下：找一条上限线，把超过它的列压到线上，线以下的列原样保留。
/// 这样「一列长句子 + 四列短数字」只会削那一列，不会把数字列一起削断。
fn shrink(desired: &[u32], total: u32, min_outer: u32) -> Vec<u32> {
    let capped = |cap: u32| -> u64 {
        desired
            .iter()
            .map(|width| u64::from((*width).min(cap).max(min_outer)))
            .sum()
    };
    let mut low = min_outer;
    let mut high = desired
        .iter()
        .copied()
        .max()
        .unwrap_or(min_outer)
        .max(min_outer);
    while low < high {
        let mid = low + (high - low + 1) / 2;
        if capped(mid) <= u64::from(total) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    desired
        .iter()
        .map(|width| (*width).min(low).max(min_outer))
        .collect()
}

/// 把和硬掰到 `total`：多退少补都落在最宽的列上，肉眼看不出来，也不会把某列
/// 挤到 0。
fn settle(widths: &mut [u32], total: u32) {
    if widths.is_empty() {
        return;
    }
    loop {
        let sum = widths.iter().map(|width| u64::from(*width)).sum::<u64>();
        match sum.cmp(&u64::from(total)) {
            std::cmp::Ordering::Equal => return,
            std::cmp::Ordering::Less => {
                let gap = (u64::from(total) - sum).min(u64::from(u32::MAX)) as u32;
                let widest = widest_index(widths);
                widths[widest] = widths[widest].saturating_add(gap);
            }
            std::cmp::Ordering::Greater => {
                let excess = (sum - u64::from(total)).min(u64::from(u32::MAX)) as u32;
                let widest = widest_index(widths);
                let take = excess.min(widths[widest].saturating_sub(1));
                if take == 0 {
                    return;
                }
                widths[widest] -= take;
            }
        }
    }
}

fn widest_index(widths: &[u32]) -> usize {
    widths
        .iter()
        .enumerate()
        .max_by_key(|(index, width)| (**width, std::cmp::Reverse(*index)))
        .map(|(index, _)| index)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOTAL: u32 = 960;
    const PAD: u32 = 14;
    const MIN: u32 = 60;

    fn sum(widths: &[u32]) -> u32 {
        widths.iter().sum()
    }

    #[test]
    fn widths_always_add_up_to_the_table_width() {
        for natural in [
            vec![10, 200, 170, 90, 10],
            vec![0, 0, 0],
            vec![5000],
            vec![900, 900, 900, 900],
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        ] {
            let widths = plan_table_columns(&natural, TOTAL, PAD, MIN);
            assert_eq!(sum(&widths), TOTAL, "natural = {natural:?}");
            assert_eq!(widths.len(), natural.len());
        }
    }

    #[test]
    fn wide_content_column_beats_a_one_character_column() {
        // 赞助榜的形状:排名/赞助人/QQ/金额/笔数。等分时每列 192,九位 QQ 号
        // (~170 px)减掉 28 px padding 就装不下——这个用例锁住「QQ 列比序号列宽」。
        let widths = plan_table_columns(&[24, 200, 170, 90, 24], TOTAL, PAD, MIN);
        assert!(
            widths[2] >= 170 + PAD * 2,
            "QQ 列应当容得下整串号码: {widths:?}"
        );
        assert!(widths[1] > widths[0], "赞助人列应当比排名列宽: {widths:?}");
        assert!(widths[4] < widths[3], "笔数列不该比金额列还宽: {widths:?}");
    }

    #[test]
    fn narrow_columns_survive_one_overlong_column() {
        // 一列长句子 + 三列短数字:该削的只有长句子那列。短列停在保底宽度
        // (min_content + 两侧 padding),彼此完全一样,长列吃掉剩下的全部。
        let widths = plan_table_columns(&[2400, 40, 40, 40], TOTAL, PAD, MIN);
        let floor = MIN + PAD * 2;
        for index in 1..4 {
            assert_eq!(widths[index], floor, "短列不该被长列牵连: {widths:?}");
        }
        assert_eq!(widths[0], TOTAL - floor * 3, "长列吃掉剩下的: {widths:?}");
        assert_eq!(sum(&widths), TOTAL);
    }

    #[test]
    fn every_column_stays_wider_than_its_padding() {
        // 列数多到 min_content 摆不下时也不能出现负内容宽。
        let natural = vec![300_u32; 20];
        let widths = plan_table_columns(&natural, TOTAL, PAD, MIN);
        assert_eq!(sum(&widths), TOTAL);
        for width in &widths {
            assert!(*width > 0, "{widths:?}");
        }
    }

    #[test]
    fn equal_content_still_splits_evenly() {
        let widths = plan_table_columns(&[120, 120, 120, 120], TOTAL, PAD, MIN);
        assert_eq!(sum(&widths), TOTAL);
        let spread = widths.iter().max().unwrap() - widths.iter().min().unwrap();
        assert!(spread <= 1, "内容一样宽的列应当几乎等分: {widths:?}");
    }
}
