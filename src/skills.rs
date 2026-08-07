use serde_json::{Map, Value};
use worker::D1Database;

use crate::db::{self, now_iso};
use crate::error::ApiResult;
use crate::models::{ActionInput, SkillAction, SkillDef, SkillRow, SkillTtl};

pub fn deploy_skill() -> SkillDef {
    SkillDef {
        skill_id: "deploy.result".to_string(),
        template: "{{service}} 在 {{env}} 部署{{status}}".to_string(),
        facts_schema: vec!["service".into(), "status".into(), "env".into()],
        actions: vec![
            SkillAction {
                id: "rollback".into(),
                risk: "destructive".into(),
                confirm: true,
                title: "回滚".into(),
                payload: None,
            },
            SkillAction {
                id: "ack".into(),
                risk: "low".into(),
                confirm: false,
                title: "已知晓".into(),
                payload: None,
            },
        ],
        ttl: SkillTtl {
            default_sec: 86_400,
            destructive_sec: 1_800,
        },
        version: Some(1),
    }
}

pub async fn seed_skill(db: &D1Database) -> ApiResult<()> {
    let existing: Option<SkillRow> = db::first(
        db,
        "SELECT skill_id, template, facts_schema_json, actions_json, ttl_json FROM skills WHERE skill_id = ?",
        vec![db::text("deploy.result")],
    )
    .await?;
    if existing.is_none() {
        upsert_skill(db, &deploy_skill()).await?;
    }
    Ok(())
}

pub fn normalize_skill(mut skill: SkillDef) -> SkillDef {
    skill.ttl.default_sec = skill.ttl.default_sec.clamp(1, 86_400);
    skill.ttl.destructive_sec = skill.ttl.destructive_sec.clamp(1, 1_800);
    skill
}

pub fn skill_from_row(row: SkillRow) -> ApiResult<SkillDef> {
    Ok(normalize_skill(SkillDef {
        skill_id: row.skill_id,
        template: row.template,
        facts_schema: serde_json::from_str(&row.facts_schema_json)?,
        actions: serde_json::from_str(&row.actions_json)?,
        ttl: serde_json::from_str(&row.ttl_json)?,
        version: None,
    }))
}

pub async fn get_skill(db: &D1Database, skill_id: &str) -> ApiResult<Option<SkillDef>> {
    let row: Option<SkillRow> = db::first(
        db,
        "SELECT skill_id, template, facts_schema_json, actions_json, ttl_json FROM skills WHERE skill_id = ?",
        vec![db::text(skill_id)],
    )
    .await?;
    row.map(skill_from_row).transpose()
}

pub async fn list_skills(db: &D1Database) -> ApiResult<Vec<SkillDef>> {
    let rows: Vec<SkillRow> = db::all(
        db,
        "SELECT skill_id, template, facts_schema_json, actions_json, ttl_json FROM skills ORDER BY skill_id",
        vec![],
    )
    .await?;
    rows.into_iter().map(skill_from_row).collect()
}

pub async fn upsert_skill(db: &D1Database, input: &SkillDef) -> ApiResult<SkillDef> {
    let skill = normalize_skill(input.clone());
    if skill.skill_id.trim().is_empty() || skill.template.trim().is_empty() {
        return Err(crate::error::ApiError::validation(
            "skill_id and template are required",
        ));
    }
    let created_at = now_iso();
    db::run(
        db,
        "INSERT INTO skills (skill_id, template, facts_schema_json, actions_json, ttl_json, created_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(skill_id) DO UPDATE SET template = excluded.template, facts_schema_json = excluded.facts_schema_json, actions_json = excluded.actions_json, ttl_json = excluded.ttl_json",
        vec![
            db::text(&skill.skill_id),
            db::text(&skill.template),
            db::text(&serde_json::to_string(&skill.facts_schema)?),
            db::text(&serde_json::to_string(&skill.actions)?),
            db::text(&serde_json::to_string(&skill.ttl)?),
            db::text(&created_at),
        ],
    )
    .await?;
    Ok(skill)
}

pub fn render_summary(template: &str, facts: &Map<String, Value>) -> String {
    let mut output = String::with_capacity(template.len());
    let chars: Vec<char> = template.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if index + 3 < chars.len() && chars[index] == '{' && chars[index + 1] == '{' {
            let mut end = None;
            for cursor in (index + 2)..chars.len().saturating_sub(1) {
                if chars[cursor] == '}' && chars[cursor + 1] == '}' {
                    end = Some(cursor);
                    break;
                }
            }
            if let Some(end) = end {
                let key: String = chars[index + 2..end]
                    .iter()
                    .collect::<String>()
                    .trim()
                    .to_string();
                if let Some(value) = facts.get(&key) {
                    match value {
                        Value::String(value) => output.push_str(value),
                        Value::Null => {}
                        other => output.push_str(&other.to_string()),
                    }
                }
                index = end + 2;
                continue;
            }
        }
        output.push(chars[index]);
        index += 1;
    }
    output
}

pub fn to_voice_script(summary: &str) -> String {
    let value = summary.trim();
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 80 {
        return value.to_string();
    }
    chars[..79].iter().collect::<String>() + "…"
}

pub fn action_needs_confirm(action: &SkillAction) -> bool {
    action.risk == "destructive" || action.confirm
}

pub fn resolve_actions(skill: &SkillDef, requested: Option<&[ActionInput]>) -> Vec<SkillAction> {
    let Some(requested) = requested else {
        return Vec::new();
    };
    let mut output = Vec::new();
    for item in requested {
        match item {
            ActionInput::Key(id) => {
                if let Some(action) = skill.actions.iter().find(|action| action.id == *id) {
                    output.push(action.clone());
                }
            }
            ActionInput::Definition(input) => {
                if input.id.trim().is_empty() {
                    continue;
                }
                let base = skill.actions.iter().find(|action| action.id == input.id);
                let risk = input
                    .risk
                    .clone()
                    .or_else(|| base.map(|action| action.risk.clone()))
                    .unwrap_or_else(|| "low".into());
                let confirm = input
                    .confirm
                    .or_else(|| base.map(|action| action.confirm))
                    .unwrap_or(risk == "destructive");
                output.push(SkillAction {
                    id: input.id.clone(),
                    risk,
                    confirm,
                    title: input
                        .title
                        .clone()
                        .or_else(|| base.map(|action| action.title.clone()))
                        .unwrap_or_else(|| input.id.clone()),
                    payload: input
                        .payload
                        .clone()
                        .or_else(|| base.and_then(|action| action.payload.clone())),
                });
            }
        }
    }
    output
}
