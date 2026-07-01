use super::{empty_parameters, ToolRegistry, ToolSpec};
use rand::{seq::SliceRandom, Rng};
use serde_json::{json, Value};

const ZHOUYI_HEXAGRAMS: &[&str] = &[
    "乾为天",
    "坤为地",
    "水雷屯",
    "山水蒙",
    "水天需",
    "天水讼",
    "地水师",
    "水地比",
    "风天小畜",
    "天泽履",
    "地天泰",
    "天地否",
    "天火同人",
    "火天大有",
    "地山谦",
    "雷地豫",
    "泽雷随",
    "山风蛊",
    "地泽临",
    "风地观",
    "火雷噬嗑",
    "山火贲",
    "山地剥",
    "地雷复",
    "天雷无妄",
    "山天大畜",
    "山雷颐",
    "泽风大过",
    "坎为水",
    "离为火",
    "泽山咸",
    "雷风恒",
    "天山遁",
    "雷天大壮",
    "火地晋",
    "地火明夷",
    "风火家人",
    "火泽睽",
    "水山蹇",
    "雷水解",
    "山泽损",
    "风雷益",
    "泽天夬",
    "天风姤",
    "泽地萃",
    "地风升",
    "泽水困",
    "水风井",
    "泽火革",
    "火风鼎",
    "震为雷",
    "艮为山",
    "风山渐",
    "雷泽归妹",
    "雷火丰",
    "火山旅",
    "巽为风",
    "兑为泽",
    "风水涣",
    "水泽节",
    "风泽中孚",
    "雷山小过",
    "水火既济",
    "火水未济",
];

const TAROT_CARDS: &[&str] = &[
    "愚者",
    "魔术师",
    "女祭司",
    "皇后",
    "皇帝",
    "教皇",
    "恋人",
    "战车",
    "力量",
    "隐士",
    "命运之轮",
    "正义",
    "倒吊人",
    "死神",
    "节制",
    "恶魔",
    "高塔",
    "星星",
    "月亮",
    "太阳",
    "审判",
    "世界",
    "权杖王牌",
    "权杖二",
    "权杖三",
    "权杖四",
    "权杖五",
    "权杖六",
    "权杖七",
    "权杖八",
    "权杖九",
    "权杖十",
    "权杖侍从",
    "权杖骑士",
    "权杖王后",
    "权杖国王",
    "圣杯王牌",
    "圣杯二",
    "圣杯三",
    "圣杯四",
    "圣杯五",
    "圣杯六",
    "圣杯七",
    "圣杯八",
    "圣杯九",
    "圣杯十",
    "圣杯侍从",
    "圣杯骑士",
    "圣杯王后",
    "圣杯国王",
    "宝剑王牌",
    "宝剑二",
    "宝剑三",
    "宝剑四",
    "宝剑五",
    "宝剑六",
    "宝剑七",
    "宝剑八",
    "宝剑九",
    "宝剑十",
    "宝剑侍从",
    "宝剑骑士",
    "宝剑王后",
    "宝剑国王",
    "星币王牌",
    "星币二",
    "星币三",
    "星币四",
    "星币五",
    "星币六",
    "星币七",
    "星币八",
    "星币九",
    "星币十",
    "星币侍从",
    "星币骑士",
    "星币王后",
    "星币国王",
];

const FORTUNE_DIVINATIONS: &[(&str, &str)] = &[
    ("大吉", "运势的最高峰，万事亨通，求财、姻缘、事业皆极顺遂。"),
    ("吉", "稳健的吉利状态。运势持久。"),
    (
        "中吉",
        "非常不错的好运，虽不如大吉猛烈，但平稳且有上升空间。",
    ),
    (
        "小吉",
        "有小小的福气或幸运，平淡中带有喜悦，不宜过分强求大富大贵。",
    ),
    ("末吉", "吉利的末尾。意味着目前处于谷底。"),
    (
        "凶",
        "运势不佳，容易遇到阻碍或犯错，需要格外小心谨慎、反省自身。",
    ),
    ("大凶", "最差、最险恶的运势。多伴随口舌、破财或灾祸。"),
];

pub fn register(registry: &mut ToolRegistry) {
    registry.register(ToolSpec::new(
        "draw_zhouyi_hexagram",
        "随机抽取一个周易六十四卦卦名。适用于起卦、周易占卜、卦象相关请求。工具只负责随机抽取，不负责解释。",
        empty_parameters(),
        |_| async { Ok(choice(ZHOUYI_HEXAGRAMS).to_string()) },
    ));
    registry.register(ToolSpec::new(
        "draw_tarot_card",
        "随机抽取一张完整 78 张塔罗牌中的牌，并随机给出正位或逆位。工具只负责抽牌，不负责解释。",
        empty_parameters(),
        |_| async {
            let card = choice(TAROT_CARDS);
            let orientation = choice(&["正位", "逆位"]);
            Ok(format!("{card}（{orientation}）"))
        },
    ));
    registry.register(ToolSpec::new(
        "draw_fortune_lot",
        "Run a random fortune divination and return the fortune level plus meaning. Use for fortune, luck, or mysticism requests. / 随机进行一次吉凶占，返回吉凶等级和含义。适用于占卜吉凶、运势、玄学相关请求。工具只负责随机给出吉凶，不负责解释。",
        empty_parameters(),
        |_| async {
            let (luck, meaning) = choice(FORTUNE_DIVINATIONS);
            Ok(format!("{luck}：{meaning}"))
        },
    ));
    registry.register(ToolSpec::new(
        "roll_dice",
        "Roll dice and return each roll plus totals. Use for dice rolling, random tabletop checks, d6/d20, or 掷骰子 requests. / 掷骰子并返回每颗骰子点数和总和。适用于骰子、跑团检定、d6/d20 等请求。",
        json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer", "description": "Number of dice, default 1, max 100. / 骰子数量，默认 1，最多 100。" },
                "sides": { "type": "integer", "description": "Sides per die, default 6, max 1000. / 每颗骰子的面数，默认 6，最多 1000。" },
                "modifier": { "type": "integer", "description": "Optional total modifier. / 可选总和修正值。" }
            },
            "additionalProperties": false
        }),
        |args| async move { roll_dice(args) },
    ));
}

fn roll_dice(args: Value) -> anyhow::Result<String> {
    let count = args
        .get("count")
        .and_then(Value::as_u64)
        .unwrap_or(1)
        .clamp(1, 100);
    let sides = args
        .get("sides")
        .and_then(Value::as_u64)
        .unwrap_or(6)
        .clamp(2, 1000);
    let modifier = args.get("modifier").and_then(Value::as_i64).unwrap_or(0);
    let mut rng = rand::thread_rng();
    let rolls = (0..count)
        .map(|_| rng.gen_range(1..=sides) as i64)
        .collect::<Vec<_>>();
    let total = rolls.iter().sum::<i64>();
    Ok(json!({
        "ok": true,
        "count": count,
        "sides": sides,
        "rolls": rolls,
        "total": total,
        "modifier": modifier,
        "modified_total": total + modifier,
    })
    .to_string())
}

fn choice<T>(items: &'static [T]) -> &'static T {
    items
        .choose(&mut rand::thread_rng())
        .expect("xuanxue data must not be empty")
}
