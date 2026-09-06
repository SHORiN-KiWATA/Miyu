//! 唤醒词编码:唤醒词 → KWS 模型的拼音 token 行。
//!
//! wenetspeech KWS 模型的建模单元是"声母 + 带声调韵母"(如
//! `x iǎo m ǐ x iǎo m ǐ @小米小米`)。这里不硬编码汉语音系,而是把
//! 每个字的带调全拼按模型 tokens.txt 里实际存在的单元做最长匹配拆分,
//! tokens.txt 变了也能跟着走。
//!
//! 三种写法(09-05 起):
//!
//! | 写法 | 例子 | 处理 |
//! |---|---|---|
//! | 汉字 | `未有未有` | 每字注带调拼音,一行 |
//! | 假名 / 拉丁 | `みゆみゆ`、`miyumiyu` | 假名先转罗马音,再按日语音节切分映射到近似拼音;声调未知,同一个词展开成轻声 + 一到四声几条候选行,任一命中即唤醒 |
//! | 显式拼音 | `mi1 yu2 mi1 yu2` | 空格分隔的"拼音+声调数字"(0 或不带数字 = 轻声),原样编码 |
//!
//! 一个词里可以混写(`小miyu`):汉字定调,其余部分展开声调候选。模型是普通话
//! 模型,非中文只是近似发音;能不能稳定命中要靠 `miyu-voice test` 实测。

use anyhow::{bail, Context, Result};
use pinyin::ToPinyin;
use std::collections::HashSet;
use std::path::Path;

/// 声母表,按长度降序保证 zh/ch/sh 先于 z/c/s 命中。
const INITIALS: [&str; 23] = [
    "zh", "ch", "sh", "b", "p", "m", "f", "d", "t", "n", "l", "g", "k", "h", "j", "q", "x", "r",
    "z", "c", "s", "y", "w",
];

/// 非中文音节展开的声调候选顺序:0 = 轻声(无调号)。
const TONE_VARIANTS: [u8; 5] = [1, 2, 3, 4, 0];

/// 把唤醒词编码成 keywords_buf 的若干行:`token token … @原文`。
///
/// 汉字与显式拼音只出一行;含假名/拉丁的词按声调候选展开成多行(同一个
/// `@原文`,任一行命中都算这个词)。
pub fn encode_keyword(keyword: &str, tokens_file: &Path) -> Result<Vec<String>> {
    let tokens = load_tokens(tokens_file)
        .with_context(|| format!("读取 KWS tokens: {}", tokens_file.display()))?;
    encode_with_tokens(keyword, &tokens)
}

fn encode_with_tokens(keyword: &str, tokens: &HashSet<String>) -> Result<Vec<String>> {
    let keyword = keyword.trim();
    if keyword.is_empty() {
        bail!("唤醒词为空");
    }
    if let Some(explicit) = parse_explicit_pinyin(keyword) {
        let mut parts = Vec::new();
        for (base, tone) in explicit {
            let syllable = with_tone(&base, tone);
            parts.extend(
                split_syllable(&syllable, tokens)
                    .with_context(|| format!("「{syllable}」无法映射到唤醒模型的建模单元"))?,
            );
        }
        return Ok(vec![format!(
            "{} @{}",
            parts.join(" "),
            phrase_tag(keyword)
        )]);
    }

    // 逐段分类:汉字(定调)/ 假名与拉丁(待定调)。
    let mut units: Vec<Unit> = Vec::new();
    let mut latin = String::new();
    let flush_latin = |latin: &mut String, units: &mut Vec<Unit>| -> Result<()> {
        if latin.is_empty() {
            return Ok(());
        }
        for syllable in romaji_to_syllables(latin)? {
            units.push(Unit::Toneless(syllable));
        }
        latin.clear();
        Ok(())
    };
    for ch in keyword.chars() {
        if ch.is_whitespace() {
            flush_latin(&mut latin, &mut units)?;
            continue;
        }
        if let Some(pinyin) = ch.to_pinyin() {
            flush_latin(&mut latin, &mut units)?;
            units.push(Unit::Fixed(pinyin.with_tone().to_string()));
        } else if let Some(romaji) = kana_to_romaji(ch, latin.as_str()) {
            match romaji {
                Kana::Syllable(text) => latin.push_str(text),
                Kana::Youon(suffix) => {
                    // 拗音:前一个 i 段音节去掉 i 接 ya/yu/yo(ki+ya=kya,shi+ya=sha)。
                    let trimmed = latin.trim_end_matches('i').to_string();
                    latin = trimmed;
                    if latin.ends_with("sh") || latin.ends_with("ch") || latin.ends_with('j') {
                        latin.push_str(&suffix[1..]); // sh+a / ch+u / j+o
                    } else {
                        latin.push_str(suffix);
                    }
                }
                Kana::Skip => {}
            }
        } else if ch.is_ascii_alphabetic() {
            latin.push(ch.to_ascii_lowercase());
        } else if ch == '-' || ch == '\'' || ch == 'ー' || ch == '・' {
            // 长音符/分隔符:忽略。
        } else {
            bail!("唤醒词只支持汉字、假名、拉丁字母,「{ch}」无法注音");
        }
    }
    flush_latin(&mut latin, &mut units)?;
    if units.is_empty() {
        bail!("唤醒词没有可编码的内容");
    }

    let has_toneless = units.iter().any(|unit| matches!(unit, Unit::Toneless(_)));
    let mut lines = Vec::new();
    let tones: &[u8] = if has_toneless { &TONE_VARIANTS } else { &[0] };
    let mut last_error: Option<anyhow::Error> = None;
    for &tone in tones {
        let mut parts = Vec::new();
        let mut ok = true;
        for unit in &units {
            let syllable = match unit {
                Unit::Fixed(syllable) => syllable.clone(),
                Unit::Toneless(base) => with_tone(base, tone),
            };
            match split_syllable(&syllable, tokens) {
                Ok(tokens) => parts.extend(tokens),
                Err(error) => {
                    last_error =
                        Some(error.context(format!("「{syllable}」无法映射到唤醒模型的建模单元")));
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            let line = format!("{} @{}", parts.join(" "), phrase_tag(keyword));
            if !lines.contains(&line) {
                lines.push(line);
            }
        }
    }
    if lines.is_empty() {
        return Err(last_error.unwrap_or_else(|| anyhow::anyhow!("唤醒词无法编码")));
    }
    Ok(lines)
}

/// `@` 后面的标签:sherpa 按空白切 token,标签里不能有空格。
fn phrase_tag(keyword: &str) -> String {
    keyword.split_whitespace().collect::<Vec<_>>().join("_")
}

enum Unit {
    /// 汉字:带调全拼已定。
    Fixed(String),
    /// 假名/拉丁:无调拼音音节,编码时展开声调候选。
    Toneless(String),
}

enum Kana {
    Syllable(&'static str),
    /// 小写 ゃゅょ:与前一个音节拼成拗音,值为 "ya" / "yu" / "yo"。
    Youon(&'static str),
    /// 促音/长音等不发独立音的字符。
    Skip,
}

/// 假名 → 罗马音(平假名与片假名同表)。`_previous` 目前不用,留给需要
/// 上下文的规则(如 ん 的鼻音变体)。
fn kana_to_romaji(ch: char, _previous: &str) -> Option<Kana> {
    // 片假名折到平假名(U+30A1..U+30F6 ↔ U+3041..U+3096)。
    let code = ch as u32;
    let ch = if (0x30A1..=0x30F6).contains(&code) {
        char::from_u32(code - 0x60).unwrap_or(ch)
    } else {
        ch
    };
    let syllable = match ch {
        'あ' => "a",
        'い' => "i",
        'う' => "u",
        'え' => "e",
        'お' => "o",
        'か' => "ka",
        'き' => "ki",
        'く' => "ku",
        'け' => "ke",
        'こ' => "ko",
        'さ' => "sa",
        'し' => "shi",
        'す' => "su",
        'せ' => "se",
        'そ' => "so",
        'た' => "ta",
        'ち' => "chi",
        'つ' => "tsu",
        'て' => "te",
        'と' => "to",
        'な' => "na",
        'に' => "ni",
        'ぬ' => "nu",
        'ね' => "ne",
        'の' => "no",
        'は' => "ha",
        'ひ' => "hi",
        'ふ' => "fu",
        'へ' => "he",
        'ほ' => "ho",
        'ま' => "ma",
        'み' => "mi",
        'む' => "mu",
        'め' => "me",
        'も' => "mo",
        'や' => "ya",
        'ゆ' => "yu",
        'よ' => "yo",
        'ら' => "ra",
        'り' => "ri",
        'る' => "ru",
        'れ' => "re",
        'ろ' => "ro",
        'わ' => "wa",
        'を' => "wo",
        'ん' => "n",
        'が' => "ga",
        'ぎ' => "gi",
        'ぐ' => "gu",
        'げ' => "ge",
        'ご' => "go",
        'ざ' => "za",
        'じ' => "ji",
        'ず' => "zu",
        'ぜ' => "ze",
        'ぞ' => "zo",
        'だ' => "da",
        'ぢ' => "ji",
        'づ' => "zu",
        'で' => "de",
        'ど' => "do",
        'ば' => "ba",
        'び' => "bi",
        'ぶ' => "bu",
        'べ' => "be",
        'ぼ' => "bo",
        'ぱ' => "pa",
        'ぴ' => "pi",
        'ぷ' => "pu",
        'ぺ' => "pe",
        'ぽ' => "po",
        'ゃ' => return Some(Kana::Youon("ya")),
        'ゅ' => return Some(Kana::Youon("yu")),
        'ょ' => return Some(Kana::Youon("yo")),
        'っ' | 'ぁ' | 'ぃ' | 'ぅ' | 'ぇ' | 'ぉ' | 'ゎ' => return Some(Kana::Skip),
        _ => return None,
    };
    Some(Kana::Syllable(syllable))
}

/// 日语音节(罗马音)→ 近似拼音音节候选(按顺序试,第一个能编码的用)。
/// 拉丁输入也走这张表:`miyumiyu` → mi yu mi yu。
fn romaji_syllables() -> &'static [(&'static str, &'static [&'static str])] {
    &[
        // 三字母优先(最长匹配)。
        ("shi", &["xi"]),
        ("chi", &["qi"]),
        ("tsu", &["cu"]),
        ("kya", &["qia"]),
        ("kyu", &["qiu"]),
        ("kyo", &["qiu"]),
        ("sha", &["xia"]),
        ("shu", &["xiu"]),
        ("sho", &["xiu"]),
        ("cha", &["qia"]),
        ("chu", &["qiu"]),
        ("cho", &["qiu"]),
        ("hya", &["xia"]),
        ("hyu", &["xiu"]),
        ("hyo", &["xiu"]),
        ("rya", &["lia"]),
        ("ryu", &["liu"]),
        ("ryo", &["liu"]),
        ("gya", &["jia"]),
        ("gyu", &["jiu"]),
        ("gyo", &["jiu"]),
        ("nya", &["nia"]),
        ("nyu", &["niu"]),
        ("nyo", &["niu"]),
        ("mya", &["mia"]),
        ("myu", &["miu"]),
        ("myo", &["miu"]),
        ("bya", &["bia"]),
        ("byu", &["biu"]),
        ("byo", &["biu"]),
        ("pya", &["pia"]),
        ("pyu", &["piu"]),
        ("pyo", &["piu"]),
        // 两字母。
        ("ka", &["ka"]),
        ("ki", &["qi"]),
        ("ku", &["ku"]),
        ("ke", &["kei", "ke"]),
        ("ko", &["kou", "ke"]),
        ("sa", &["sa"]),
        ("su", &["su"]),
        ("se", &["sei", "se"]),
        ("so", &["sou"]),
        ("ta", &["ta"]),
        ("te", &["tei", "te"]),
        ("to", &["tou"]),
        ("na", &["na"]),
        ("ni", &["ni"]),
        ("nu", &["nu"]),
        ("ne", &["nei", "ne"]),
        ("no", &["nou"]),
        ("ha", &["ha"]),
        ("hi", &["xi"]),
        ("fu", &["fu"]),
        ("he", &["hei", "he"]),
        ("ho", &["hou"]),
        ("ma", &["ma"]),
        ("mi", &["mi"]),
        ("mu", &["mu"]),
        ("me", &["mei", "me"]),
        ("mo", &["mou"]),
        // ゆ [jɯ] 更接近普通话 you 而不是 yu(09-05 TTS 实测:中文口音的
        // miyumiyu 被识别成"米有米游",KWS 也只认 mi/you 的组合)。
        ("ya", &["ya"]),
        ("yu", &["you", "yu"]),
        ("yo", &["you"]),
        ("ra", &["la"]),
        ("ri", &["li"]),
        ("ru", &["lu"]),
        ("re", &["lei", "le"]),
        ("ro", &["lou"]),
        ("la", &["la"]),
        ("li", &["li"]),
        ("lu", &["lu"]),
        ("le", &["lei", "le"]),
        ("lo", &["lou"]),
        ("wa", &["wa"]),
        ("wo", &["wo"]),
        ("wu", &["wu"]),
        ("wi", &["wei"]),
        ("we", &["wei"]),
        ("ga", &["ga"]),
        ("gi", &["ji"]),
        ("gu", &["gu"]),
        ("ge", &["gei", "ge"]),
        ("go", &["gou"]),
        ("za", &["za"]),
        ("ji", &["ji"]),
        ("zu", &["zu"]),
        ("ze", &["zei", "ze"]),
        ("zo", &["zou"]),
        ("da", &["da"]),
        ("di", &["di"]),
        ("du", &["du"]),
        ("de", &["dei", "de"]),
        ("do", &["dou"]),
        ("ba", &["ba"]),
        ("bi", &["bi"]),
        ("bu", &["bu"]),
        ("be", &["bei"]),
        ("bo", &["bo"]),
        ("pa", &["pa"]),
        ("pi", &["pi"]),
        ("pu", &["pu"]),
        ("pe", &["pei"]),
        ("po", &["po"]),
        ("ja", &["jia"]),
        ("ju", &["jiu"]),
        ("jo", &["jiu"]),
        ("xi", &["xi"]),
        ("qi", &["qi"]),
        ("yi", &["yi"]),
        // 单元音。
        ("a", &["a"]),
        ("i", &["yi"]),
        ("u", &["wu"]),
        ("e", &["e", "ei"]),
        ("o", &["wo", "ou"]),
        ("n", &["en"]),
    ]
}

/// 罗马音串 → 无调拼音音节(最长匹配切分)。返回每个音节的候选列表里
/// 排第一的;编码时若失败再试后备,见 [`with_tone`] 调用处。
fn romaji_to_syllables(latin: &str) -> Result<Vec<String>> {
    let table = romaji_syllables();
    let mut out = Vec::new();
    let mut rest = latin;
    while !rest.is_empty() {
        // 双写辅音(っ 的罗马音,如 tt)→ 丢掉第一个。
        let bytes = rest.as_bytes();
        if bytes.len() >= 2
            && bytes[0] == bytes[1]
            && !b"aeiou".contains(&bytes[0])
            && bytes[0] != b'n'
        {
            rest = &rest[1..];
            continue;
        }
        let hit = table
            .iter()
            .filter(|(romaji, _)| rest.starts_with(romaji))
            .max_by_key(|(romaji, _)| romaji.len());
        let Some((romaji, candidates)) = hit else {
            bail!("「{rest}」无法按音节切分(支持日语罗马音写法,如 miyu、shiro)");
        };
        out.push(candidates.join("|"));
        rest = &rest[romaji.len()..];
    }
    Ok(out)
}

/// 显式拼音写法:全部为 ASCII 的 `拼音[声调数字]` 用空格分隔,至少两个音节
/// 或带数字。返回 (无调音节, 声调)。
fn parse_explicit_pinyin(keyword: &str) -> Option<Vec<(String, u8)>> {
    if !keyword.is_ascii() || !keyword.contains(' ') && !keyword.chars().any(|c| c.is_ascii_digit())
    {
        return None;
    }
    let mut out = Vec::new();
    for part in keyword.split_whitespace() {
        let (base, tone) = match part.char_indices().find(|(_, c)| c.is_ascii_digit()) {
            Some((index, digit)) => {
                let tone = digit.to_digit(10)? as u8;
                if tone > 5 || index + 1 != part.len() {
                    return None;
                }
                (&part[..index], if tone == 5 { 0 } else { tone })
            }
            None => (part, 0),
        };
        if base.is_empty() || !base.chars().all(|c| c.is_ascii_lowercase() || c == 'v') {
            return None;
        }
        out.push((base.replace('v', "ü"), tone));
    }
    (!out.is_empty()).then_some(out)
}

/// 给无调音节加声调符号(0 = 不加)。标调规则:有 a/e 标 a/e;ou 标 o;
/// 其余标最后一个元音。`base` 可以是 `a|b` 形式的候选串,只处理第一个候选;
/// 后备候选由 [`split_syllable`] 失败后重试(见 `split_syllable_any`)。
fn with_tone(base: &str, tone: u8) -> String {
    let candidates: Vec<&str> = base.split('|').collect();
    candidates
        .iter()
        .map(|candidate| mark_tone(candidate, tone))
        .collect::<Vec<_>>()
        .join("|")
}

fn mark_tone(base: &str, tone: u8) -> String {
    if tone == 0 {
        return base.to_string();
    }
    let vowels = ['a', 'e', 'i', 'o', 'u', 'ü'];
    let chars: Vec<char> = base.chars().collect();
    let target = if let Some(index) = chars.iter().position(|c| *c == 'a' || *c == 'e') {
        index
    } else if let Some(index) = base.find("ou") {
        base[..index].chars().count()
    } else if let Some(index) = chars.iter().rposition(|c| vowels.contains(c)) {
        index
    } else {
        return base.to_string();
    };
    let toned = match (chars[target], tone) {
        ('a', 1) => 'ā',
        ('a', 2) => 'á',
        ('a', 3) => 'ǎ',
        ('a', 4) => 'à',
        ('e', 1) => 'ē',
        ('e', 2) => 'é',
        ('e', 3) => 'ě',
        ('e', 4) => 'è',
        ('i', 1) => 'ī',
        ('i', 2) => 'í',
        ('i', 3) => 'ǐ',
        ('i', 4) => 'ì',
        ('o', 1) => 'ō',
        ('o', 2) => 'ó',
        ('o', 3) => 'ǒ',
        ('o', 4) => 'ò',
        ('u', 1) => 'ū',
        ('u', 2) => 'ú',
        ('u', 3) => 'ǔ',
        ('u', 4) => 'ù',
        ('ü', 1) => 'ǖ',
        ('ü', 2) => 'ǘ',
        ('ü', 3) => 'ǚ',
        ('ü', 4) => 'ǜ',
        (other, _) => other,
    };
    chars
        .iter()
        .enumerate()
        .map(|(index, c)| if index == target { toned } else { *c })
        .collect()
}

fn load_tokens(path: &Path) -> Result<HashSet<String>> {
    let raw = std::fs::read_to_string(path)?;
    Ok(raw
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .map(str::to_string)
        .collect())
}

/// 音节(可为 `a|b` 候选串)→ 建模单元;候选逐个试,第一个成功的算。
fn split_syllable(syllable: &str, tokens: &HashSet<String>) -> Result<Vec<String>> {
    let mut last = None;
    for candidate in syllable.split('|') {
        match split_single(candidate, tokens) {
            Ok(parts) => return Ok(parts),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("音节为空")))
}

fn split_single(syllable: &str, tokens: &HashSet<String>) -> Result<Vec<String>> {
    for initial in INITIALS {
        if let Some(rest) = syllable.strip_prefix(initial) {
            if !rest.is_empty() && tokens.contains(initial) && tokens.contains(rest) {
                return Ok(vec![initial.to_string(), rest.to_string()]);
            }
        }
    }
    // 零声母音节(如 ài、ér)整体就是一个 token。
    if tokens.contains(syllable) {
        return Ok(vec![syllable.to_string()]);
    }
    bail!("音节 {syllable} 不在模型词表中");
}

/// 形式校验(配置界面用,本进程不一定有模型词表):汉字、假名、拉丁字母、
/// 显式拼音之外的字符一律拒绝。
pub fn looks_encodable(keyword: &str) -> bool {
    let keyword = keyword.trim();
    !keyword.is_empty()
        && keyword.chars().all(|ch| {
            ch.is_whitespace()
                || ch.to_pinyin().is_some()
                || kana_to_romaji(ch, "").is_some()
                || ch.is_ascii_alphanumeric()
                || matches!(ch, '-' | '\'' | 'ー' | '・')
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tokens_file(entries: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for (index, entry) in entries.iter().enumerate() {
            writeln!(file, "{entry} {index}").unwrap();
        }
        file
    }

    #[test]
    fn encodes_initial_final_pairs() {
        let file = tokens_file(&["w", "èi", "y", "ǒu"]);
        let lines = encode_keyword("未有未有", file.path()).unwrap();
        assert_eq!(lines, vec!["w èi y ǒu w èi y ǒu @未有未有"]);
    }

    #[test]
    fn zero_initial_uses_whole_syllable() {
        let file = tokens_file(&["ài", "x", "iǎo"]);
        let lines = encode_keyword("小爱", file.path()).unwrap();
        assert_eq!(lines, vec!["x iǎo ài @小爱"]);
    }

    #[test]
    fn latin_expands_tone_variants() {
        let file = tokens_file(&["m", "y", "i", "u", "ī", "í", "ǐ", "ì", "ū", "ú", "ǔ", "ù"]);
        let lines = encode_keyword("miyumiyu", file.path()).unwrap();
        assert_eq!(lines.len(), 5, "{lines:?}");
        assert_eq!(lines[0], "m ī y ū m ī y ū @miyumiyu");
        assert_eq!(lines[4], "m i y u m i y u @miyumiyu");
    }

    #[test]
    fn kana_transliterates_like_latin() {
        let file = tokens_file(&["m", "y", "ī", "ū"]);
        let lines = encode_keyword("みゆみゆ", file.path()).unwrap();
        // 只有一声能编码(词表里没有别的调),其余候选静默跳过。
        assert_eq!(lines, vec!["m ī y ū m ī y ū @みゆみゆ"]);
        let katakana = encode_keyword("ミユミユ", file.path()).unwrap();
        assert_eq!(katakana[0], "m ī y ū m ī y ū @ミユミユ");
    }

    #[test]
    fn youon_and_sokuon() {
        let file = tokens_file(&["x", "iā", "iū", "q", "k", "ū", "t", "ā"]);
        assert_eq!(
            encode_keyword("しゃ", file.path()).unwrap()[0],
            "x iā @しゃ"
        );
        assert_eq!(
            encode_keyword("きゅ", file.path()).unwrap()[0],
            "q iū @きゅ"
        );
        assert_eq!(
            encode_keyword("かった", file.path()).unwrap()[0],
            "k ā t ā @かった"
        );
    }

    #[test]
    fn explicit_pinyin_with_tone_digits() {
        let file = tokens_file(&["m", "y", "ǐ", "ú", "i", "u"]);
        assert_eq!(
            encode_keyword("mi3 yu2 mi3 yu2", file.path()).unwrap(),
            vec!["m ǐ y ú m ǐ y ú @mi3_yu2_mi3_yu2"]
        );
        assert_eq!(
            encode_keyword("mi yu", file.path()).unwrap(),
            vec!["m i y u @mi_yu"]
        );
    }

    #[test]
    fn mixed_han_and_latin() {
        let file = tokens_file(&["x", "iǎo", "m", "ī", "y", "ū"]);
        assert_eq!(
            encode_keyword("小miyu", file.path()).unwrap(),
            vec!["x iǎo m ī y ū @小miyu"]
        );
    }

    #[test]
    fn rejects_unsupported_characters() {
        let file = tokens_file(&["m", "ǐ"]);
        assert!(encode_keyword("米!", file.path()).is_err());
        assert!(encode_keyword("한글", file.path()).is_err());
        assert!(!looks_encodable("米!"));
        assert!(looks_encodable("miyumiyu"));
        assert!(looks_encodable("みゆみゆ"));
        assert!(looks_encodable("密友密友"));
    }

    #[test]
    fn rejects_out_of_vocabulary_syllable() {
        let file = tokens_file(&["m"]);
        assert!(encode_keyword("米", file.path()).is_err());
    }

    #[test]
    fn tone_marks_land_on_the_right_vowel() {
        assert_eq!(mark_tone("xiao", 3), "xiǎo");
        assert_eq!(mark_tone("you", 2), "yóu");
        assert_eq!(mark_tone("liu", 4), "liù");
        assert_eq!(mark_tone("mei", 1), "mēi");
        assert_eq!(mark_tone("mi", 0), "mi");
    }
}
