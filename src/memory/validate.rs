//! 组织器输出的校验。
//!
//! 记忆的增删改由模型提议（见 [`super::organizer`]），但**提议不等于执行**：
//! 每一条都要过这里。归属与可见性尤其严——模型可以建议公开一条事实，不能把别
//! 人的私有记忆改掉（`validate_knowledge_update_scope`）。
//!
//! 校验失败一律丢弃这一条而不是整批：一条格式不对不该让整轮组织白跑。

use crate::memory::*;

pub(crate) const MAX_ORGANIZED_ITEMS: usize = 20;

pub(crate) fn load_existing_memory_candidates(
    conn: &Connection,
    source_diaries: &[ShortDiaryRecord],
) -> Result<Vec<ExistingMemoryRecord>> {
    let mut allowed_principals = BTreeSet::new();
    let mut privileged_source = false;
    for diary in source_diaries {
        match diary.origin.principal_ownership() {
            Some(ownership) => {
                allowed_principals.insert(ownership.owner_principal);
            }
            None => privileged_source = true,
        }
    }
    let query = source_diaries
        .iter()
        .flat_map(|diary| [&diary.user_message, &diary.assistant_message])
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join("\n");
    let tokens = query_tokens_with_limit(&query, 256);
    let mut scored = Vec::<(f32, ExistingMemoryRecord)>::new();
    let mut facts = conn.prepare(
        "SELECT id, content, truth_status, visibility, owner_principal, owner_display_name FROM facts
         WHERE status!='forgotten' AND truth_status!='rejected'
         ORDER BY updated_at DESC LIMIT 5000",
    )?;
    let rows = facts.query_map([], |row| {
        Ok(ExistingMemoryRecord {
            id: row.get(0)?,
            kind: "knowledge".to_string(),
            content: row.get(1)?,
            truth_status: row.get(2)?,
            visibility: row.get(3)?,
            owner_principal: row.get(4)?,
            owner_display_name: row.get(5)?,
        })
    })?;
    for row in rows {
        let memory = row?;
        if !organizer_candidate_is_visible(&memory, &allowed_principals, privileged_source) {
            continue;
        }
        let score = score_text(&memory.content, "", &tokens);
        if score > 0.0 {
            scored.push((score, memory));
        }
    }
    drop(facts);

    let mut diaries = conn.prepare(
        "SELECT id, content, visibility, owner_principal, owner_display_name FROM episodes
         WHERE retention='long_term' AND status!='forgotten'
         ORDER BY updated_at DESC LIMIT 5000",
    )?;
    let rows = diaries.query_map([], |row| {
        Ok(ExistingMemoryRecord {
            id: row.get(0)?,
            kind: "long_diary".to_string(),
            content: row.get(1)?,
            truth_status: "accepted".to_string(),
            visibility: row.get(2)?,
            owner_principal: row.get(3)?,
            owner_display_name: row.get(4)?,
        })
    })?;
    for row in rows {
        let memory = row?;
        if !organizer_candidate_is_visible(&memory, &allowed_principals, privileged_source) {
            continue;
        }
        let score = score_text(&memory.content, "", &tokens);
        if score > 0.0 {
            scored.push((score, memory));
        }
    }
    scored.sort_by(|left, right| {
        right
            .0
            .partial_cmp(&left.0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut fact_count = 0usize;
    let mut diary_count = 0usize;
    Ok(scored
        .into_iter()
        .filter_map(|(_, memory)| match memory.kind.as_str() {
            "knowledge" if fact_count < 30 => {
                fact_count += 1;
                Some(memory)
            }
            "long_diary" if diary_count < 20 => {
                diary_count += 1;
                Some(memory)
            }
            _ => None,
        })
        .collect())
}

pub(crate) fn organizer_candidate_is_visible(
    memory: &ExistingMemoryRecord,
    allowed_principals: &BTreeSet<String>,
    privileged_source: bool,
) -> bool {
    match memory.visibility.as_str() {
        VISIBILITY_PUBLIC => true,
        VISIBILITY_PRINCIPAL => allowed_principals.contains(&memory.owner_principal),
        VISIBILITY_PRIVILEGED => privileged_source,
        _ => false,
    }
}

pub(crate) fn validate_knowledge_action(
    action: &KnowledgeAction,
    diary_ids: &BTreeSet<i64>,
    candidate_fact_ids: &BTreeSet<i64>,
) -> Result<()> {
    if !matches!(action.operation.as_str(), "create" | "update") {
        bail!("invalid knowledge operation");
    }
    if action.operation == "update"
        && !action
            .target_id
            .is_some_and(|id| candidate_fact_ids.contains(&id))
    {
        bail!("knowledge update target is not an allowed candidate");
    }
    if action.operation == "create" && action.target_id.is_some() {
        bail!("new knowledge must not have a target id");
    }
    if !matches!(
        action.memory_type.as_str(),
        "fact" | "preference" | "relationship" | "task" | "self" | "other"
    ) {
        bail!("invalid knowledge type");
    }
    if !matches!(
        action.truth_status.as_str(),
        "accepted" | "reported" | "uncertain" | "fictional" | "rejected"
    ) {
        bail!("invalid knowledge truth status");
    }
    validate_organized_content(&action.content, 2_000)?;
    validate_evidence_ids(&action.diary_ids, diary_ids)?;
    if !(1..=5).contains(&action.importance)
        || !action.confidence.is_finite()
        || !(0.0..=1.0).contains(&action.confidence)
    {
        bail!("knowledge importance or confidence is out of range");
    }
    Ok(())
}

pub(crate) fn validate_knowledge_visibility(
    batch: &OrganizationBatch,
    action: &KnowledgeAction,
) -> Result<()> {
    if !matches!(
        action.visibility.as_str(),
        "" | VISIBILITY_PUBLIC | VISIBILITY_PRINCIPAL | VISIBILITY_PRIVILEGED
    ) {
        bail!("invalid knowledge visibility");
    }
    let target_visibility = action.target_id.and_then(|target_id| {
        batch
            .existing
            .iter()
            .find(|memory| memory.kind == "knowledge" && memory.id == target_id)
            .map(|memory| memory.visibility.as_str())
    });
    if target_visibility
        .is_some_and(|target| !action.visibility.is_empty() && action.visibility != target)
    {
        bail!("knowledge updates cannot change memory visibility");
    }
    let effective_visibility = target_visibility.unwrap_or(action.visibility.as_str());
    if effective_visibility == VISIBILITY_PUBLIC && action.memory_type != "fact" {
        bail!("only general facts may become public memories");
    }
    validate_memory_subjects(batch, &action.diary_ids, &action.subjects)?;
    if effective_visibility == VISIBILITY_PUBLIC {
        if !action.subjects.is_empty() {
            bail!("public memories cannot contain person subjects");
        }
        let content = action.content.to_lowercase();
        for diary in batch
            .diaries
            .iter()
            .filter(|diary| action.diary_ids.contains(&diary.id))
        {
            for marker in [
                diary.origin.sender_id.trim(),
                diary.origin.sender_display_name.trim(),
            ] {
                if marker.chars().count() >= 2 && content.contains(&marker.to_lowercase()) {
                    bail!("public memory content contains a source identity marker");
                }
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_knowledge_update_scope(
    batch: &OrganizationBatch,
    action: &KnowledgeAction,
    candidates: &BTreeMap<i64, &ExistingMemoryRecord>,
) -> Result<()> {
    let Some(target_id) = action.target_id else {
        return Ok(());
    };
    let target = candidates
        .get(&target_id)
        .context("knowledge update target disappeared from candidates")?;
    let evidence = diary_ownership(batch, &action.diary_ids);
    let allowed = match target.visibility.as_str() {
        VISIBILITY_PUBLIC => true,
        VISIBILITY_PRINCIPAL => {
            evidence.visibility == VISIBILITY_PRINCIPAL
                && evidence.owner_principal == target.owner_principal
        }
        VISIBILITY_PRIVILEGED => evidence.visibility == VISIBILITY_PRIVILEGED,
        _ => false,
    };
    if !allowed {
        bail!("knowledge update evidence belongs to a different principal");
    }
    Ok(())
}

pub(crate) fn validate_long_diary(
    batch: &OrganizationBatch,
    diary: &LongDiaryDraft,
    diary_ids: &BTreeSet<i64>,
) -> Result<()> {
    validate_organized_content(&diary.content, 3_000)?;
    validate_evidence_ids(&diary.diary_ids, diary_ids)?;
    if !(1..=5).contains(&diary.importance)
        || !diary.confidence.is_finite()
        || !(0.0..=1.0).contains(&diary.confidence)
    {
        bail!("long diary importance or confidence is out of range");
    }
    if !matches!(
        diary.visibility.as_str(),
        "" | VISIBILITY_PRINCIPAL | VISIBILITY_PRIVILEGED
    ) {
        bail!("long diaries cannot be public memories");
    }
    validate_memory_subjects(batch, &diary.diary_ids, &diary.subjects)?;
    Ok(())
}

pub(crate) fn validate_memory_subjects(
    batch: &OrganizationBatch,
    diary_ids: &[i64],
    subjects: &[MemorySubject],
) -> Result<()> {
    if subjects.len() > 32 {
        bail!("organized memory contains too many subjects");
    }
    let allowed_principals = batch
        .diaries
        .iter()
        .filter(|diary| diary_ids.contains(&diary.id))
        .filter_map(|diary| diary.owner_principal.as_deref())
        .collect::<BTreeSet<_>>();
    for subject in subjects {
        let principal = subject
            .principal
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let name = subject
            .name
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if principal.is_none() && name.is_none() {
            bail!("memory subject must contain a principal or name");
        }
        if principal.is_some_and(|value| !allowed_principals.contains(value)) {
            bail!("memory subject references an untrusted principal");
        }
        if name
            .is_some_and(|value| value.chars().count() > 128 || value.chars().any(char::is_control))
        {
            bail!("memory subject name is invalid");
        }
    }
    Ok(())
}

pub(crate) fn knowledge_ownership(
    batch: &OrganizationBatch,
    action: &KnowledgeAction,
) -> MemoryOwnership {
    if let Some(target) = action.target_id.and_then(|target| {
        batch
            .existing
            .iter()
            .find(|memory| memory.kind == "knowledge" && memory.id == target)
    }) {
        return MemoryOwnership {
            visibility: match target.visibility.as_str() {
                VISIBILITY_PUBLIC => VISIBILITY_PUBLIC,
                VISIBILITY_PRINCIPAL => VISIBILITY_PRINCIPAL,
                _ => VISIBILITY_PRIVILEGED,
            },
            owner_principal: target.owner_principal.clone(),
            owner_display_name: target.owner_display_name.clone(),
        };
    }
    if action.visibility == VISIBILITY_PUBLIC && action.memory_type == "fact" {
        return MemoryOwnership::public();
    }
    diary_ownership(batch, &action.diary_ids)
}

pub(crate) fn diary_ownership(batch: &OrganizationBatch, diary_ids: &[i64]) -> MemoryOwnership {
    let mut principals = BTreeMap::<String, String>::new();
    let mut privileged_source = false;
    for id in diary_ids {
        let Some(diary) = batch.diaries.iter().find(|diary| diary.id == *id) else {
            privileged_source = true;
            continue;
        };
        match diary.origin.principal_ownership() {
            Some(ownership) => {
                principals
                    .entry(ownership.owner_principal)
                    .or_insert(ownership.owner_display_name);
            }
            None => privileged_source = true,
        }
    }
    if !privileged_source && principals.len() == 1 {
        let (principal, display_name) = principals
            .into_iter()
            .next()
            .expect("one principal was checked");
        MemoryOwnership::principal(principal, display_name)
    } else {
        MemoryOwnership::privileged()
    }
}

/// 整理产物继承哪个会话的标记。和 `diary_ownership` 同一把尺子：只有全部
/// 来源都指向同一个会话时才敢盖章，否则留空——盖错会让一次会话级重置删掉
/// 别的会话的东西，那比少删一条严重得多。
pub(crate) fn organized_session_id(batch: &OrganizationBatch, diary_ids: &[i64]) -> String {
    let mut sessions = BTreeSet::<&str>::new();
    for id in diary_ids {
        let Some(diary) = batch.diaries.iter().find(|diary| diary.id == *id) else {
            return String::new();
        };
        let session = diary.origin.session_id.trim();
        if session.is_empty() {
            return String::new();
        }
        sessions.insert(session);
    }
    match sessions.len() {
        1 => sessions
            .into_iter()
            .next()
            .expect("one session was checked")
            .to_string(),
        _ => String::new(),
    }
}

pub(crate) fn validate_organized_content(content: &str, max_chars: usize) -> Result<()> {
    let content = content.trim();
    if content.is_empty() || content.chars().count() > max_chars || content.contains('\0') {
        bail!("organized memory content is empty or too long");
    }
    Ok(())
}

pub(crate) fn validate_evidence_ids(ids: &[i64], allowed: &BTreeSet<i64>) -> Result<()> {
    if ids.is_empty() || ids.iter().any(|id| !allowed.contains(id)) {
        bail!("organized memory references invalid diary ids");
    }
    Ok(())
}

pub(crate) fn normalized_ids_json(ids: &[i64]) -> String {
    serde_json::to_string(&ids.iter().copied().collect::<BTreeSet<_>>()).unwrap_or("[]".to_string())
}

pub(crate) fn ownership_subjects_json(ownership: &MemoryOwnership) -> String {
    if ownership.visibility != VISIBILITY_PRINCIPAL {
        return "[]".to_string();
    }
    serde_json::to_string(&[MemorySubject {
        principal: Some(ownership.owner_principal.clone()),
        name: (!ownership.owner_display_name.trim().is_empty())
            .then(|| truncate_chars(&compact_line(&ownership.owner_display_name), 128)),
    }])
    .unwrap_or_else(|_| "[]".to_string())
}

pub(crate) fn organized_subjects_json(
    batch: &OrganizationBatch,
    diary_ids: &[i64],
    declared: &[MemorySubject],
    ownership: &MemoryOwnership,
) -> String {
    if ownership.visibility == VISIBILITY_PUBLIC {
        return "[]".to_string();
    }
    let mut subjects = declared
        .iter()
        .map(|subject| MemorySubject {
            principal: subject
                .principal
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            name: subject
                .name
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
        })
        .collect::<BTreeSet<_>>();
    for diary in batch
        .diaries
        .iter()
        .filter(|diary| diary_ids.contains(&diary.id))
    {
        if let Some(principal) = diary.owner_principal.as_ref() {
            subjects.insert(MemorySubject {
                principal: Some(principal.clone()),
                name: (!diary.origin.sender_display_name.trim().is_empty())
                    .then(|| truncate_chars(&compact_line(&diary.origin.sender_display_name), 128)),
            });
        }
    }
    serde_json::to_string(&subjects).unwrap_or_else(|_| "[]".to_string())
}

pub(crate) fn normalized_tags_json(tags: &[String]) -> String {
    let tags = tags
        .iter()
        .map(|tag| compact_line(tag))
        .filter(|tag| !tag.is_empty() && tag.chars().count() <= 32)
        .take(8)
        .collect::<BTreeSet<_>>();
    serde_json::to_string(&tags).unwrap_or("[]".to_string())
}
