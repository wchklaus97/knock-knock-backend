mod action_effects;
mod apns;
mod audit;
mod auth;
mod commands;
mod db;
mod error;
mod history;
mod models;
mod outbox;
mod pagination;
mod phone_operations;
mod providers;
mod push;
mod rate_limits;
mod realtime;
mod reminders;
mod sessions;
mod skills;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{rc::Rc, time::Duration};
use worker::*;

use crate::auth::{
    bearer_token, config_value, create_agent_for_user, ensure_supabase_user, hash_api_key,
    hash_password, issue_user_auth, mint_api_key, mint_pairing_code, new_id, require_agent,
    require_user, require_user_or_agent, runtime_configuration, sha256_hex, supabase_auth_enabled,
    supabase_auth_response, supabase_get_user, supabase_logout, supabase_refresh, supabase_sign_in,
    supabase_sign_up, verify_password,
};
use crate::error::{ApiError, ApiResult};
use crate::models::{
    ActionResultRequest, AuthCredentials, CommandEnvelope, CreateAgentRequest, DeviceRequest,
    EventRequest, PairingClaimRequest, PairingCodeRequest, PhoneConfirmRequest, PhoneReplyRequest,
    PhoneSessionUpdateRequest, ProgressRequest, RefreshRequest, SessionRequest,
};

#[derive(Debug, Deserialize)]
struct IdOnly {
    #[serde(rename = "id")]
    _id: String,
}

#[derive(Debug, Deserialize)]
struct RefreshRow {
    id: String,
    user_id: String,
    expires_at: String,
    revoked_at: Option<String>,
    email: String,
}

#[derive(Debug, Deserialize)]
struct ConfirmationRequest {
    confirmation_token: String,
}

#[event(fetch)]
pub async fn main(req: Request, env: Env, _ctx: Context) -> Result<Response> {
    let origin = req.headers().get("origin").ok().flatten();
    let request_id = request_id();
    let result = dispatch(req, env.clone()).await;
    let mut response = match result {
        Ok(response) => response,
        Err(error) => {
            let retry_after = error.retry_after;
            let mut response = error.with_request_id(&request_id).response()?;
            if let Some(retry_after) = retry_after {
                response
                    .headers_mut()
                    .set("Retry-After", &retry_after.to_string())?;
            }
            response
        }
    };
    response.headers_mut().set("X-Request-ID", &request_id)?;
    add_common_headers(response, &env, origin)
}

/// Runs the durable Outbox worker. The handler only marks a command successful
/// after the domain/provider adapter returns a result; unavailable external
/// adapters remain unknown/retryable instead of being reported as success.
#[event(scheduled)]
pub async fn run_scheduled_outbox(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    if runtime_configuration(&env).is_err() {
        return;
    }
    if let Ok(db) = env.d1("DB") {
        let _ = outbox::drain(&db, &env).await;
        let _ = reminders::drain_due(&db, &env).await;
        let _ = history::purge_expired_all(&db, &env).await;
    }
}

async fn dispatch(mut req: Request, env: Env) -> ApiResult<Response> {
    let path = req.path();
    let method = req.method();
    runtime_configuration(&env)?;
    let action_provider_config = providers::load(&env)?;

    if method == Method::Options {
        return Ok(Response::empty()?);
    }
    if path == "/health" && method == Method::Get {
        return json_response(
            json!({
                "ok": true,
                "runtime": "cloudflare-worker",
                "api": "rust",
                "version": config_value(&env, "SERVICE_VERSION", "unknown"),
                "push_mode": config_value(&env, "PUSH_MODE", "dev"),
                "apns_ready": crate::apns::is_ready(&env),
                "apns_production": config_value(&env, "APNS_PRODUCTION", "false") == "true",
                "action_provider_mode": action_provider_config.mode().as_str(),
                "action_provider_ready": providers::ready(&action_provider_config),
                "action_reminder_enabled": action_provider_config.enabled("create_reminder"),
                "action_message_enabled": action_provider_config.enabled("send_message"),
            }),
            200,
        );
    }
    if path == "/metrics" && method == Method::Get {
        return Ok(Response::ok(
            "# HELP knock_knock_api_info API runtime information\n\
             # TYPE knock_knock_api_info gauge\n\
             knock_knock_api_info{runtime=\"cloudflare-worker\",api=\"rust\"} 1\n",
        )?);
    }

    let db = env.d1("DB")?;
    let segments = path_segments(&path);
    let identity = rate_limit_identity(&req)?;
    rate_limits::enforce(&db, &path, &identity).await?;

    match (method, segments.as_slice()) {
        (Method::Post, ["v1", "auth", "register"]) => auth_register(&mut req, &env, &db).await,
        (Method::Post, ["v1", "auth", "login"]) => auth_login(&mut req, &env, &db).await,
        (Method::Post, ["v1", "auth", "refresh"]) => auth_refresh(&mut req, &env, &db).await,
        (Method::Post, ["v1", "auth", "logout"]) => auth_logout(&mut req, &env, &db).await,

        (Method::Get, ["v1", "agents"]) => list_agents(&req, &env, &db).await,
        (Method::Post, ["v1", "agents"]) => create_agent(&mut req, &env, &db).await,
        (Method::Post, ["v1", "agents", agent_id, "rotate-key"]) => {
            rotate_agent_key(&req, &env, &db, agent_id).await
        }
        (Method::Get, ["v1", "agents", "me", "actions", "pending"]) => {
            list_agent_pending(&req, &db).await
        }

        (Method::Post, ["v1", "pairing", "code"]) => create_pairing_code(&mut req, &env, &db).await,
        (Method::Get, ["v1", "pairing", "code", code]) => {
            pairing_status(&req, &env, &db, code).await
        }
        (Method::Post, ["v1", "pairing", "claim"]) => pairing_claim(&mut req, &env, &db).await,

        (Method::Get, ["v1", "skills"]) => list_skills(&req, &env, &db).await,
        (Method::Post, ["v1", "skills"]) => upsert_skill(&mut req, &env, &db).await,

        (Method::Post, ["v1", "sessions"]) => create_session(&mut req, &env, &db).await,
        (Method::Get, ["v1", "sessions", session_id]) => {
            get_session(&req, &env, &db, session_id).await
        }
        (Method::Post, ["v1", "sessions", session_id, "progress"]) => {
            update_session_progress(&mut req, &env, &db, session_id).await
        }
        (Method::Post, ["v1", "sessions", session_id, "events"]) => {
            report_session_event(&mut req, &env, &db, session_id).await
        }
        (Method::Get, ["v1", "sessions", session_id, "actions", "pending"]) => {
            list_session_pending(&req, &db, session_id).await
        }
        (Method::Post, ["v1", "actions", action_id, "result"]) => {
            submit_action_result(&mut req, &db, action_id).await
        }

        (Method::Get, ["v1", "phone", "sessions"]) => phone_sessions(&req, &env, &db).await,
        (Method::Get, ["v1", "phone", "sync"]) => phone_sync(&req, &env, &db).await,
        (Method::Get, ["v1", "phone", "events"]) => phone_events(&req, &env, db).await,
        (Method::Get, ["v1", "phone", "sessions", session_id]) => {
            phone_session_detail(&req, &env, &db, session_id).await
        }
        (Method::Patch, ["v1", "phone", "sessions", session_id]) => {
            phone_update_session(&mut req, &env, &db, session_id).await
        }
        (Method::Delete, ["v1", "phone", "sessions", session_id]) => {
            phone_delete_session(&req, &env, &db, session_id).await
        }
        (Method::Get, ["v1", "phone", "sessions", session_id, "history"]) => {
            phone_history(&req, &env, &db, session_id).await
        }
        (Method::Get, ["v1", "phone", "sessions", session_id, "messages"]) => {
            phone_messages(&req, &env, &db, session_id).await
        }
        (Method::Get, ["v1", "phone", "sessions", session_id, "export"]) => {
            phone_export(&req, &env, &db, session_id).await
        }
        (Method::Post, ["v1", "phone", "devices"]) => register_device(&mut req, &env, &db).await,
        (Method::Post, ["v1", "phone", "sessions", session_id, "reply"]) => {
            phone_reply(&mut req, &env, &db, session_id).await
        }
        (Method::Post, ["v1", "phone", "sessions", session_id, "confirm"]) => {
            phone_confirm(&mut req, &env, &db, session_id).await
        }
        (Method::Post, ["v1", "phone", "commands"]) => {
            phone_create_command(&mut req, &env, &db).await
        }
        (Method::Get, ["v1", "phone", "commands"]) => phone_list_commands(&req, &env, &db).await,
        (Method::Get, ["v1", "phone", "commands", command_id]) => {
            phone_get_command(&req, &env, &db, command_id).await
        }
        (Method::Post, ["v1", "phone", "commands", command_id, "confirm"]) => {
            phone_confirm_command(&mut req, &env, &db, command_id).await
        }
        (Method::Post, ["v1", "phone", "commands", command_id, "cancel"]) => {
            phone_cancel_command(&req, &env, &db, command_id).await
        }
        (Method::Post, ["v1", "phone", "commands", command_id, "undo"]) => {
            phone_undo_command(&req, &env, &db, command_id).await
        }
        (Method::Get, ["v1", "phone", "models", model_id]) => {
            phone_model_descriptor(&req, &env, &db, model_id).await
        }
        (Method::Get, ["v1", "phone", "retrievals", retrieval_id, "download"]) => {
            phone_retrieval_download(&req, &env, &db, retrieval_id).await
        }
        (Method::Get, ["v1", "phone", "search"]) => phone_search(&req, &env, &db).await,
        (Method::Post, ["v1", "phone", "pushes", push_id, "read"]) => {
            phone_mark_push_read(&req, &env, &db, push_id).await
        }
        (Method::Post, ["v1", "phone", "pushes", push_id, "dismiss"]) => {
            phone_dismiss_push(&req, &env, &db, push_id).await
        }
        (Method::Post, ["v1", "phone", "pushes", "read-all"]) => {
            phone_mark_all_pushes_read(&req, &env, &db).await
        }
        (Method::Get, ["v1", "dev", "pushes"]) => dev_pushes(&req, &env, &db).await,
        _ => Err(ApiError::not_found("Route not found")),
    }
}

fn path_segments(path: &str) -> Vec<&str> {
    path.trim_matches('/')
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect()
}

fn request_id() -> String {
    let timestamp = worker::Date::now().as_millis();
    let mut entropy = [0_u8; 8];
    if getrandom::fill(&mut entropy).is_err() {
        return format!("req_{timestamp:x}");
    }
    let suffix = entropy
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("req_{timestamp:x}_{suffix}")
}

fn rate_limit_identity(request: &Request) -> ApiResult<String> {
    if let Some(value) = request.headers().get("cf-connecting-ip")? {
        if !value.trim().is_empty() {
            return Ok(format!("edge:{}", value.trim()));
        }
    }
    if let Some(value) = request.headers().get("x-forwarded-for")? {
        if let Some(first) = value.split(',').next().map(str::trim) {
            if !first.is_empty() {
                return Ok(format!("edge:{first}"));
            }
        }
    }
    Ok("edge:anonymous".into())
}

async fn read_json<T: DeserializeOwned>(request: &mut Request) -> ApiResult<T> {
    request
        .json()
        .await
        .map_err(|_| ApiError::validation("Request body must be valid JSON"))
}

fn json_response(value: Value, status: u16) -> ApiResult<Response> {
    Ok(Response::from_json(&value)?.with_status(status))
}

fn query_value(request: &Request, name: &str) -> ApiResult<Option<String>> {
    let url = request.url()?;
    Ok(url
        .query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned()))
}

fn query_limit(request: &Request, default: i32) -> ApiResult<i32> {
    Ok(query_value(request, "limit")?
        .and_then(|value| value.parse::<i32>().ok())
        .unwrap_or(default)
        .clamp(1, 200))
}

fn query_claim(request: &Request) -> ApiResult<bool> {
    Ok(!matches!(
        query_value(request, "claim")?
            .unwrap_or_else(|| "true".into())
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no"
    ))
}

fn request_metadata(request: &Request) -> ApiResult<(Option<String>, Option<String>)> {
    let user_agent = request.headers().get("user-agent")?;
    let ip_address = request
        .headers()
        .get("x-forwarded-for")?
        .and_then(|value| value.split(',').next().map(str::trim).map(str::to_string));
    Ok((user_agent, ip_address))
}

fn validate_credentials(input: &AuthCredentials) -> ApiResult<String> {
    let email = input.email.trim().to_ascii_lowercase();
    if !email.contains('@') || email.len() < 5 || email.len() > 320 {
        return Err(ApiError::validation("email must be a valid email address"));
    }
    if input.password.len() < 8 || input.password.len() > 128 {
        return Err(ApiError::validation(
            "password must be between 8 and 128 characters",
        ));
    }
    Ok(email)
}

fn map_supabase_login_error(error: ApiError) -> ApiError {
    if error.status >= 500 {
        return error;
    }
    ApiError::unauthorized("Invalid credentials")
}

fn map_supabase_refresh_error(error: ApiError) -> ApiError {
    if error.status >= 500 {
        return error;
    }
    ApiError::unauthorized("Invalid or expired refresh token")
}

fn map_supabase_register_error(error: ApiError) -> ApiError {
    if error.status >= 500 {
        return error;
    }
    let message = error.message.to_ascii_lowercase();
    if message.contains("already") || message.contains("registered") {
        return ApiError::conflict("Email already registered");
    }
    ApiError::validation("Unable to create account")
}

async fn auth_register(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let body: AuthCredentials = read_json(req).await?;
    let email = validate_credentials(&body)?;
    if supabase_auth_enabled(env) {
        let session = supabase_sign_up(env, &email, &body.password)
            .await
            .map_err(map_supabase_register_error)?;
        let remote = session.user.clone().ok_or_else(|| {
            ApiError::new(
                502,
                "supabase_auth_error",
                "Supabase did not return the new user",
            )
        })?;
        let remote_email = remote.email.as_deref().unwrap_or(&email);
        let user = ensure_supabase_user(db, &remote.id, remote_email).await?;
        let auth = supabase_auth_response(&user.user_id, &session)?;
        audit::record_audit(
            db,
            "auth.register",
            Some(&user.user_id),
            None,
            None,
            json!({ "email": remote_email, "provider": "supabase" }),
        )
        .await;
        return json_response(auth, 201);
    }
    if db::first::<IdOnly>(
        db,
        "SELECT id FROM users WHERE email = ?",
        vec![db::text(&email)],
    )
    .await?
    .is_some()
    {
        return Err(ApiError::conflict("Email already registered"));
    }
    let user_id = new_id("usr")?;
    let password_hash = hash_password(&body.password)?;
    let created = db::run(
        db,
        "INSERT INTO users (id, email, password_hash, created_at) VALUES (?, ?, ?, ?)",
        vec![
            db::text(&user_id),
            db::text(&email),
            db::text(&password_hash),
            db::text(&db::now_iso()),
        ],
    )
    .await;
    if let Err(error) = created {
        return Err(ApiError::conflict(format!(
            "Unable to register user: {}",
            error.message
        )));
    }
    let (user_agent, ip_address) = request_metadata(req)?;
    let auth = issue_user_auth(
        db,
        env,
        &user_id,
        &email,
        user_agent.as_deref(),
        ip_address.as_deref(),
    )
    .await?;
    audit::record_audit(
        db,
        "auth.register",
        Some(&user_id),
        None,
        None,
        json!({ "email": email }),
    )
    .await;
    json_response(auth, 201)
}

async fn auth_login(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let body: AuthCredentials = read_json(req).await?;
    let email = validate_credentials(&body)?;
    if supabase_auth_enabled(env) {
        let session = supabase_sign_in(env, &email, &body.password)
            .await
            .map_err(map_supabase_login_error)?;
        let remote = session.user.clone().ok_or_else(|| {
            ApiError::new(
                502,
                "supabase_auth_error",
                "Supabase did not return the authenticated user",
            )
        })?;
        let remote_email = remote.email.as_deref().unwrap_or(&email);
        let user = ensure_supabase_user(db, &remote.id, remote_email).await?;
        let auth = supabase_auth_response(&user.user_id, &session)?;
        audit::record_audit(
            db,
            "auth.login",
            Some(&user.user_id),
            None,
            None,
            json!({ "provider": "supabase" }),
        )
        .await;
        return json_response(auth, 200);
    }
    let user = db::first::<models::UserRow>(
        db,
        "SELECT id, email, password_hash FROM users WHERE email = ?",
        vec![db::text(&email)],
    )
    .await?;
    let user = user.filter(|row| verify_password(&body.password, &row.password_hash));
    let user = user.ok_or_else(|| ApiError::unauthorized("Invalid credentials"))?;
    let (user_agent, ip_address) = request_metadata(req)?;
    let auth = issue_user_auth(
        db,
        env,
        &user.id,
        &user.email,
        user_agent.as_deref(),
        ip_address.as_deref(),
    )
    .await?;
    audit::record_audit(db, "auth.login", Some(&user.id), None, None, json!({})).await;
    json_response(auth, 200)
}

async fn auth_refresh(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let body: RefreshRequest = read_json(req).await?;
    let invalid_refresh_token = if supabase_auth_enabled(env) {
        body.refresh_token.trim().is_empty()
    } else {
        body.refresh_token.len() < 20
    };
    if invalid_refresh_token {
        return Err(ApiError::validation("refresh_token is invalid"));
    }
    if supabase_auth_enabled(env) {
        let session = supabase_refresh(env, &body.refresh_token)
            .await
            .map_err(map_supabase_refresh_error)?;
        let access_token = session.access_token.as_deref().ok_or_else(|| {
            ApiError::new(
                502,
                "supabase_auth_error",
                "Supabase did not return an access token",
            )
        })?;
        let remote = match session.user.clone() {
            Some(user) => user,
            None => supabase_get_user(env, access_token).await?,
        };
        let remote_email = remote.email.as_deref().ok_or_else(|| {
            ApiError::new(502, "supabase_auth_error", "Supabase user email is missing")
        })?;
        let user = ensure_supabase_user(db, &remote.id, remote_email).await?;
        let auth = supabase_auth_response(&user.user_id, &session)?;
        audit::record_audit(
            db,
            "auth.refresh",
            Some(&user.user_id),
            None,
            None,
            json!({ "provider": "supabase" }),
        )
        .await;
        return json_response(auth, 200);
    }
    let row = db::first::<RefreshRow>(
        db,
        "SELECT rt.id, rt.user_id, rt.expires_at, rt.revoked_at, u.email FROM refresh_tokens rt JOIN users u ON u.id = rt.user_id WHERE rt.token_hash = ?",
        vec![db::text(&sha256_hex(&body.refresh_token))],
    )
    .await?
    .ok_or_else(|| ApiError::unauthorized("Invalid or expired refresh token"))?;
    if row.revoked_at.is_some() || db::is_expired(&row.expires_at) {
        return Err(ApiError::unauthorized("Invalid or expired refresh token"));
    }
    let now = db::now_iso();
    let rotated = db::run(
        db,
        "UPDATE refresh_tokens SET revoked_at = ?, last_used_at = ? WHERE id = ? AND revoked_at IS NULL",
        vec![db::text(&now), db::text(&now), db::text(&row.id)],
    )
    .await?;
    if db::changes(&rotated) == 0 {
        return Err(ApiError::unauthorized("Refresh token was already rotated"));
    }
    let (user_agent, ip_address) = request_metadata(req)?;
    let auth = issue_user_auth(
        db,
        env,
        &row.user_id,
        &row.email,
        user_agent.as_deref(),
        ip_address.as_deref(),
    )
    .await?;
    audit::record_audit(
        db,
        "auth.refresh",
        Some(&row.user_id),
        None,
        None,
        json!({}),
    )
    .await;
    json_response(auth, 200)
}

async fn auth_logout(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let body: RefreshRequest = read_json(req).await?;
    let invalid_refresh_token = if supabase_auth_enabled(env) {
        body.refresh_token.trim().is_empty()
    } else {
        body.refresh_token.len() < 20
    };
    if invalid_refresh_token {
        return Err(ApiError::validation("refresh_token is invalid"));
    }
    if supabase_auth_enabled(env) {
        if let Some(token) = bearer_token(req)? {
            supabase_logout(env, &token).await?;
        }
        return json_response(json!({ "ok": true }), 200);
    }
    let now = db::now_iso();
    db::run(
        db,
        "UPDATE refresh_tokens SET revoked_at = ?, last_used_at = ? WHERE token_hash = ? AND revoked_at IS NULL",
        vec![
            db::text(&now),
            db::text(&now),
            db::text(&sha256_hex(&body.refresh_token)),
        ],
    )
    .await?;
    json_response(json!({ "ok": true }), 200)
}

async fn list_agents(req: &Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let rows: Vec<models::AgentRow> = db::all(
        db,
        "SELECT id, user_id, label, host_label, created_at FROM agents WHERE user_id = ? ORDER BY created_at DESC",
        vec![db::text(&user.user_id)],
    )
    .await?;
    let agents = rows
        .into_iter()
        .map(|row| {
            json!({
                "agent_id": row.id,
                "user_id": row.user_id,
                "label": row.label,
                "host_label": row.host_label,
                "created_at": row.created_at,
                "last_seen_at": Value::Null,
            })
        })
        .collect::<Vec<_>>();
    json_response(json!({ "agents": agents }), 200)
}

async fn create_agent(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body: CreateAgentRequest = read_json(req).await?;
    if body.label.trim().is_empty() {
        return Err(ApiError::validation("label is required"));
    }
    let created = create_agent_for_user(
        db,
        &user.user_id,
        body.label.trim(),
        body.host_label.as_deref(),
    )
    .await?;
    let agent_id = created
        .get("agent")
        .and_then(|agent| agent.get("agent_id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    audit::record_audit(
        db,
        "agent.create",
        Some(&user.user_id),
        Some(&agent_id),
        None,
        json!({}),
    )
    .await;
    json_response(created, 201)
}

async fn rotate_agent_key(
    req: &Request,
    env: &Env,
    db: &D1Database,
    agent_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let row = db::first::<models::AgentRow>(
        db,
        "SELECT id, user_id, label, host_label, created_at FROM agents WHERE id = ? AND user_id = ?",
        vec![db::text(agent_id), db::text(&user.user_id)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Agent not found"))?;
    let api_key = mint_api_key()?;
    db::run(
        db,
        "UPDATE agents SET api_key_hash = ? WHERE id = ? AND user_id = ?",
        vec![
            db::text(&hash_api_key(&api_key)),
            db::text(agent_id),
            db::text(&user.user_id),
        ],
    )
    .await?;
    audit::record_audit(
        db,
        "agent.rotate_key",
        Some(&user.user_id),
        Some(agent_id),
        None,
        json!({}),
    )
    .await;
    json_response(
        json!({
            "agent": {
                "agent_id": row.id,
                "user_id": row.user_id,
                "label": row.label,
                "host_label": row.host_label,
                "created_at": row.created_at,
            },
            "api_key": api_key,
        }),
        200,
    )
}

async fn create_pairing_code(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body = req
        .json::<PairingCodeRequest>()
        .await
        .unwrap_or(PairingCodeRequest { ttl_sec: None });
    let ttl = body.ttl_sec.unwrap_or(600).clamp(1, 3_600);
    let code = mint_pairing_code()?;
    let expires_at = db::add_seconds_iso(ttl);
    db::run(
        db,
        "INSERT INTO pairing_codes (code, user_id, expires_at, claimed_at, claim_token, created_at) VALUES (?, ?, ?, NULL, NULL, ?)",
        vec![
            db::text(&code),
            db::text(&user.user_id),
            db::text(&expires_at),
            db::text(&db::now_iso()),
        ],
    )
    .await?;
    audit::record_audit(
        db,
        "pairing.create",
        Some(&user.user_id),
        None,
        None,
        json!({ "expires_at": expires_at }),
    )
    .await;
    json_response(json!({ "code": code, "expires_at": expires_at }), 201)
}

async fn pairing_status(
    req: &Request,
    env: &Env,
    db: &D1Database,
    code: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let code = code.trim();
    if code.len() < 4 || code.len() > 64 {
        return Err(ApiError::validation("Invalid pairing code"));
    }
    let row = db::first::<models::PairingRow>(
        db,
        "SELECT user_id, expires_at, claimed_at FROM pairing_codes WHERE code = ? AND user_id = ?",
        vec![db::text(code), db::text(&user.user_id)],
    )
    .await?
    .ok_or_else(|| ApiError::not_found("Pairing code not found"))?;
    let status = if row.claimed_at.is_some() {
        "claimed"
    } else if db::is_expired(&row.expires_at) {
        "expired"
    } else {
        "waiting"
    };
    json_response(
        json!({
            "code": code,
            "status": status,
            "expires_at": row.expires_at,
            "claimed_at": row.claimed_at,
        }),
        200,
    )
}

async fn pairing_claim(req: &mut Request, _env: &Env, db: &D1Database) -> ApiResult<Response> {
    let body: PairingClaimRequest = read_json(req).await?;
    let code = body.code.trim();
    let label = body.label.trim();
    if code.len() < 4 || code.len() > 64 || label.is_empty() {
        return Err(ApiError::validation("code and label are required"));
    }
    let now = db::now_iso();
    let agent_id = new_id("agt")?;
    let api_key = mint_api_key()?;
    let claim_token = new_id("claim")?;
    let update = db::prepare(
        db,
        "UPDATE pairing_codes SET claimed_at = ?, claim_token = ? WHERE code = ? AND claimed_at IS NULL AND expires_at > ?",
        vec![
            db::text(&now),
            db::text(&claim_token),
            db::text(code),
            db::text(&now),
        ],
    )?;
    let insert = db::prepare(
        db,
        "INSERT INTO agents (id, user_id, label, host_label, api_key_hash, created_at) SELECT ?, user_id, ?, ?, ?, ? FROM pairing_codes WHERE code = ? AND claim_token = ? AND claimed_at = ?",
        vec![
            db::text(&agent_id),
            db::text(label),
            db::optional_text(body.host_label.as_deref()),
            db::text(&hash_api_key(&api_key)),
            db::text(&now),
            db::text(code),
            db::text(&claim_token),
            db::text(&now),
        ],
    )?;
    let results = db.batch(vec![update, insert]).await?;
    if results.get(1).map(db::changes).unwrap_or(0) == 0 {
        let pairing = db::first::<models::PairingRow>(
            db,
            "SELECT user_id, expires_at, claimed_at FROM pairing_codes WHERE code = ?",
            vec![db::text(code)],
        )
        .await?;
        return match pairing {
            None => Err(ApiError::not_found("Invalid pairing code")),
            Some(row) if row.claimed_at.is_some() => {
                Err(ApiError::conflict("Pairing code already claimed"))
            }
            Some(row) if db::is_expired(&row.expires_at) => {
                Err(ApiError::gone("Pairing code expired"))
            }
            Some(_) => Err(ApiError::conflict("Pairing code could not be claimed")),
        };
    }
    let pairing = db::first::<models::PairingRow>(
        db,
        "SELECT user_id, expires_at, claimed_at FROM pairing_codes WHERE code = ?",
        vec![db::text(code)],
    )
    .await?
    .ok_or_else(|| ApiError::new(500, "pairing_error", "Pairing owner disappeared"))?;
    audit::record_audit(
        db,
        "pairing.claim",
        Some(&pairing.user_id),
        Some(&agent_id),
        None,
        json!({ "label": label, "host_label": body.host_label }),
    )
    .await;
    json_response(
        json!({
            "agent_id": agent_id,
            "api_key": api_key,
            "label": label,
            "agent": {
                "agent_id": agent_id,
                "user_id": pairing.user_id,
                "label": label,
                "host_label": body.host_label,
                "created_at": now,
            },
        }),
        201,
    )
}

async fn list_skills(req: &Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let _ = require_user_or_agent(req, env, db).await?;
    skills::seed_skill(db).await?;
    json_response(json!({ "skills": skills::list_skills(db).await? }), 200)
}

async fn upsert_skill(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    // The registry is global and determines action permissions/risk. Until an
    // explicit admin-scoped registry exists, authenticated users and agents
    // may read it but must not overwrite another tenant's policy.
    let _ = require_user_or_agent(req, env, db).await?;
    Err(ApiError::new(
        403,
        "skill_registry_read_only",
        "The skill registry is managed by the backend release process",
    ))
}

async fn create_session(req: &mut Request, _env: &Env, db: &D1Database) -> ApiResult<Response> {
    let agent = require_agent(req, db).await?;
    skills::seed_skill(db).await?;
    let body: SessionRequest = read_json(req).await?;
    let result =
        sessions::create_or_resume_session(db, &agent.agent_id, &agent.user_id, &body).await?;
    json_response(result, 201)
}

async fn get_session(
    req: &Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let row = sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    if req.headers().get("x-agent-key")?.is_some() {
        let agent = require_agent(req, db).await?;
        if agent.agent_id != row.agent_id {
            return Err(ApiError::forbidden("Not your session"));
        }
    } else {
        let user = require_user(req, env, db).await?;
        if user.user_id != row.user_id {
            return Err(ApiError::forbidden("Not your session"));
        }
    }
    json_response(sessions::session_api(db, row).await?, 200)
}

async fn update_session_progress(
    req: &mut Request,
    _env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let agent = require_agent(req, db).await?;
    let row = sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.agent_id == agent.agent_id)
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    let body: ProgressRequest = read_json(req).await?;
    json_response(sessions::update_progress(db, &row, &body).await?, 200)
}

async fn report_session_event(
    req: &mut Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let agent = require_agent(req, db).await?;
    let row = sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.agent_id == agent.agent_id)
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    let body: EventRequest = read_json(req).await?;
    json_response(sessions::report_event(db, env, &row, &body).await?, 200)
}

async fn list_agent_pending(req: &Request, db: &D1Database) -> ApiResult<Response> {
    let agent = require_agent(req, db).await?;
    json_response(
        json!({
            "actions": sessions::pending_actions(db, &agent.agent_id, None, query_claim(req)?).await?,
        }),
        200,
    )
}

async fn list_session_pending(
    req: &Request,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let agent = require_agent(req, db).await?;
    let row = sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.agent_id == agent.agent_id)
        .filter(|row| row.deleted_at.is_none())
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    json_response(
        json!({
            "actions": sessions::pending_actions(db, &agent.agent_id, Some(&row.id), query_claim(req)?).await?,
        }),
        200,
    )
}

async fn submit_action_result(
    req: &mut Request,
    db: &D1Database,
    action_id: &str,
) -> ApiResult<Response> {
    let agent = require_agent(req, db).await?;
    let body: ActionResultRequest = read_json(req).await?;
    json_response(
        sessions::submit_action_result(
            db,
            &agent.agent_id,
            action_id,
            body.ok,
            body.message.as_deref(),
            body.output.as_ref(),
        )
        .await?,
        200,
    )
}

async fn phone_sessions(req: &Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    history::purge_expired(db, env, &user.user_id).await?;
    let before = query_value(req, "before")?;
    json_response(
        history::list_sessions(db, &user.user_id, before.as_deref(), query_limit(req, 50)?).await?,
        200,
    )
}

async fn phone_session_for_user(
    req: &Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<models::SessionRow> {
    let user = require_user(req, env, db).await?;
    sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.user_id == user.user_id && row.deleted_at.is_none())
        .ok_or_else(|| ApiError::not_found("Session not found"))
}

async fn phone_session_detail(
    req: &Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let row = phone_session_for_user(req, env, db, session_id).await?;
    let row = sessions::reconcile_waiting_session(db, row).await?;
    let retrieval_items = history::list_retrieval(db, &row.user_id, &row.id, 100).await?;
    let mut detail = sessions::session_api(db, row).await?;
    if let Value::Object(ref mut object) = detail {
        object.insert("retrieval_items".into(), Value::Array(retrieval_items));
    }
    json_response(detail, 200)
}

async fn phone_update_session(
    req: &mut Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body: PhoneSessionUpdateRequest = read_json(req).await?;
    if let Some(title) = body.title.as_deref() {
        if title.trim().len() > 200 {
            return Err(ApiError::validation("Session title is too long"));
        }
    }
    let existing = sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.user_id == user.user_id && row.deleted_at.is_none())
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    let now = db::now_iso();
    if body.title.is_none() && body.archived.is_none() {
        return phone_session_detail(req, env, db, session_id).await;
    }
    db::run(
        db,
        "UPDATE sessions SET title = CASE WHEN ? = 1 THEN ? ELSE title END, archived_at = CASE WHEN ? = 1 THEN ? ELSE archived_at END, updated_at = ? WHERE id = ? AND user_id = ? AND deleted_at IS NULL",
        vec![
            db::number(if body.title.is_some() { 1 } else { 0 }),
            db::optional_text(body.title.as_deref().map(str::trim)),
            db::number(if body.archived.is_some() { 1 } else { 0 }),
            match body.archived {
                Some(true) => db::text(&now),
                Some(false) => db::optional_text(None),
                None => db::optional_text(None),
            },
            db::text(&now),
            db::text(session_id),
            db::text(&user.user_id),
        ],
    )
    .await?;
    audit::record_audit(
        db,
        "phone.session.update",
        Some(&user.user_id),
        Some(&existing.agent_id),
        Some(session_id),
        json!({"title_changed": body.title.is_some(), "archived": body.archived}),
    )
    .await;
    phone_session_detail(req, env, db, session_id).await
}

async fn phone_delete_session(
    req: &Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let existing = sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.user_id == user.user_id)
        .ok_or_else(|| ApiError::not_found("Session not found"))?;
    let now = db::now_iso();
    let tombstone_id = new_id("tombstone")?;
    let statements = vec![
        db::prepare(
            db,
            "UPDATE sessions SET deleted_at = ?, archived_at = COALESCE(archived_at, ?), updated_at = ? WHERE id = ? AND user_id = ? AND deleted_at IS NULL",
            vec![
                db::text(&now),
                db::text(&now),
                db::text(&now),
                db::text(session_id),
                db::text(&user.user_id),
            ],
        )?,
        db::prepare(
            db,
            "INSERT OR IGNORE INTO sync_tombstones (id, user_id, entity_type, entity_id, deleted_at) VALUES (?, ?, 'session', ?, ?)",
            vec![
                db::text(&tombstone_id),
                db::text(&user.user_id),
                db::text(session_id),
                db::text(&now),
            ],
        )?,
        db::prepare(
            db,
            "UPDATE reminders SET status = 'cancelled', notification_state = 'cancelled', last_notification_error = 'session_deleted', updated_at = ? WHERE user_id = ? AND session_id = ? AND status = 'scheduled' AND provider = 'local.reminder'",
            vec![
                db::text(&now),
                db::text(&user.user_id),
                db::text(session_id),
            ],
        )?,
    ];
    let results = db.batch(statements).await?;
    if results.first().map(db::changes).unwrap_or(0) == 0 {
        return Err(ApiError::conflict("Session is already deleted"));
    }
    audit::record_audit(
        db,
        "phone.session.delete",
        Some(&user.user_id),
        Some(&existing.agent_id),
        Some(session_id),
        json!({"deleted_at": now}),
    )
    .await;
    json_response(
        json!({"ok": true, "session_id": session_id, "deleted_at": now}),
        200,
    )
}

async fn phone_messages(
    req: &Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let row = phone_session_for_user(req, env, db, session_id).await?;
    history::purge_expired(db, env, &row.user_id).await?;
    let before = query_value(req, "before")?;
    json_response(
        history::list_messages(
            db,
            &row.user_id,
            &row.id,
            before.as_deref(),
            query_limit(req, 50)?,
        )
        .await?,
        200,
    )
}

async fn phone_export(
    req: &Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let row = phone_session_for_user(req, env, db, session_id).await?;
    history::purge_expired(db, env, &row.user_id).await?;
    json_response(history::export_session(db, &row.user_id, &row).await?, 200)
}

/// Stream a retrieval payload through an authenticated Worker request. The R2
/// key never crosses the API boundary and is only resolved after checking the
/// authenticated user, live session, and retention window in D1.
async fn phone_retrieval_download(
    req: &Request,
    env: &Env,
    db: &D1Database,
    retrieval_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let retrieval = history::get_retrieval(db, &user.user_id, retrieval_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Retrieval asset not found"))?;
    let key = retrieval
        .r2_key
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ApiError::not_found("Retrieval asset is not stored"))?;
    if !history::is_user_r2_key(&user.user_id, &key) {
        return Err(ApiError::not_found("Retrieval asset not found"));
    }
    let bucket = env.bucket("R2").map_err(|_| {
        ApiError::new(
            503,
            "retrieval_storage_unavailable",
            "Retrieval storage is not configured for this environment",
        )
    })?;
    let object = bucket
        .get(key)
        .execute()
        .await
        .map_err(|_| {
            ApiError::new(
                502,
                "retrieval_storage_error",
                "Retrieval storage could not be read",
            )
        })?
        .ok_or_else(|| ApiError::not_found("Retrieval asset not found"))?;
    let body = object
        .body()
        .ok_or_else(|| {
            ApiError::new(
                502,
                "retrieval_storage_error",
                "Retrieval asset has no body",
            )
        })?
        .response_body()
        .map_err(|_| {
            ApiError::new(
                502,
                "retrieval_storage_error",
                "Retrieval asset could not be streamed",
            )
        })?;

    let headers = Headers::new();
    object.write_http_metadata(headers.clone()).map_err(|_| {
        ApiError::new(
            502,
            "retrieval_storage_error",
            "Retrieval metadata could not be read",
        )
    })?;
    if headers.get("content-type")?.is_none() {
        headers.set("content-type", "application/octet-stream")?;
    }
    // Retrievals can contain sensitive source material. Do not allow a shared
    // browser/proxy cache to outlive the authenticated request.
    headers.set("cache-control", "private, no-store")?;
    headers.set(
        "content-disposition",
        "attachment; filename=\"retrieval.bin\"",
    )?;
    headers.set("x-content-type-options", "nosniff")?;
    Ok(Response::from_body(body)?.with_headers(headers))
}

async fn phone_search(req: &Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    history::purge_expired(db, env, &user.user_id).await?;
    let query = query_value(req, "q")?.unwrap_or_default();
    json_response(
        history::search(db, &user.user_id, &query, query_limit(req, 50)?).await?,
        200,
    )
}

/// Returns signed model metadata and a short-lived artifact URL configured by
/// the release environment. The app verifies the manifest and artifact before
/// activation; this endpoint never returns provider credentials.
async fn phone_model_descriptor(
    req: &Request,
    env: &Env,
    db: &D1Database,
    model_id: &str,
) -> ApiResult<Response> {
    let _user = require_user(req, env, db).await?;
    if model_id.is_empty()
        || model_id.len() > 128
        || !model_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
    {
        return Err(ApiError::validation("Invalid model id"));
    }

    let manifest_json = config_value(env, "VOICE_MODEL_MANIFEST_JSON", "");
    let download_url = config_value(env, "VOICE_MODEL_URL", "");
    if manifest_json.trim().is_empty() || download_url.trim().is_empty() {
        return Err(ApiError::new(
            503,
            "model_unavailable",
            "This model is not configured for the current release",
        ));
    }
    if !download_url.starts_with("https://")
        && config_value(env, "ALLOW_INSECURE_MODEL_URL", "false") != "true"
    {
        return Err(ApiError::new(
            503,
            "model_unavailable",
            "The configured model URL is not secure",
        ));
    }

    let manifest: Value = serde_json::from_str(&manifest_json)
        .map_err(|_| ApiError::new(503, "model_unavailable", "The model manifest is invalid"))?;
    validate_model_manifest(&manifest, model_id)?;

    let expires_at = config_value(env, "VOICE_MODEL_EXPIRES_AT", "");
    json_response(
        json!({
            "model_id": model_id,
            "manifest": manifest,
            "download_url": download_url,
            "expires_at": if expires_at.is_empty() { Value::Null } else { Value::String(expires_at) },
        }),
        200,
    )
}

fn validate_model_manifest(manifest: &Value, model_id: &str) -> ApiResult<()> {
    let valid = manifest.get("schema_version").and_then(Value::as_i64) == Some(1)
        && manifest.get("model_id").and_then(Value::as_str) == Some(model_id)
        && manifest
            .get("model_version")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
        && manifest
            .get("sha256")
            .and_then(Value::as_str)
            .is_some_and(|value| {
                value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        && manifest
            .get("signature")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
        && manifest
            .get("size_bytes")
            .and_then(Value::as_u64)
            .is_some_and(|value| value > 0)
        && manifest
            .get("minimum_capability")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty() && value.len() <= 64);
    if !valid {
        return Err(ApiError::new(
            503,
            "model_unavailable",
            "The model manifest is invalid",
        ));
    }
    Ok(())
}

async fn phone_mark_push_read(
    req: &Request,
    env: &Env,
    db: &D1Database,
    push_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    json_response(push::mark_read(db, &user.user_id, push_id).await?, 200)
}

async fn phone_dismiss_push(
    req: &Request,
    env: &Env,
    db: &D1Database,
    push_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    json_response(push::dismiss(db, &user.user_id, push_id).await?, 200)
}

async fn phone_mark_all_pushes_read(
    req: &Request,
    env: &Env,
    db: &D1Database,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    json_response(push::mark_all_read(db, &user.user_id).await?, 200)
}

fn stream_error(error: ApiError) -> worker::Error {
    worker::Error::from(error.message)
}

fn phone_change_event_type(entity_type: &str) -> &'static str {
    match entity_type {
        "session" => "session.updated",
        "message" => "message.created",
        "command" => "command.updated",
        "push" => "push.updated",
        "retrieval" => "message.created",
        _ => "sync.required",
    }
}

fn phone_change_value(row: &models::PhoneChangeRow) -> Value {
    json!({
        "cursor": row.cursor.to_string(),
        "entity_type": row.entity_type,
        "entity_id": row.entity_id,
        "session_id": row.session_id,
        "version": row.version,
        "deleted_at": row.deleted_at,
    })
}

fn sync_cursor(request: &Request) -> ApiResult<Option<i64>> {
    let raw = query_value(request, "after")?;
    // Clients from the checkpoint release persisted the old composite cursor
    // (`timestamp|session_id`). Treat it as a replay-from-zero marker during
    // the compatibility window. The response always returns the canonical
    // numeric cursor, so the client converges after one successful sync.
    if raw.as_deref().is_some_and(|value| value.contains('|')) {
        return Ok(Some(0));
    }
    crate::realtime::parse_cursor(raw.as_deref())
        .map_err(|error| ApiError::validation(error.to_string()))
}

async fn phone_sync(req: &Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    history::purge_expired(db, env, &user.user_id).await?;
    let after = sync_cursor(req)?.unwrap_or(0);
    let limit = crate::realtime::normalize_limit(
        query_value(req, "limit")?.and_then(|value| value.parse::<i64>().ok()),
    );
    let rows: Vec<models::PhoneChangeRow> = db::all(
        db,
        "SELECT cursor, user_id, entity_type, entity_id, session_id, version, created_at, deleted_at FROM phone_changes WHERE user_id = ? AND cursor > ? ORDER BY cursor ASC LIMIT ?",
        vec![
            db::text(&user.user_id),
            db::number(after),
            db::number(limit + 1),
        ],
    )
    .await?;
    let has_more = rows.len() as i64 > limit;
    let changes = rows.into_iter().take(limit as usize).collect::<Vec<_>>();
    let cursor = changes
        .last()
        .map(|row| row.cursor)
        .unwrap_or(after)
        .to_string();
    json_response(
        json!({
            "cursor": cursor,
            "changes": changes.iter().map(phone_change_value).collect::<Vec<_>>(),
            "has_more": has_more,
        }),
        200,
    )
}

#[derive(Debug)]
struct SessionStreamState {
    db: Rc<D1Database>,
    user_id: String,
    cursor: i64,
    initial_sync: bool,
    poll_count: u8,
    close_after_emit: bool,
}

fn parse_event_cursor(request: &Request) -> ApiResult<Option<i64>> {
    let header_cursor = request.headers().get("last-event-id")?;
    let raw = query_value(request, "since")?.or(header_cursor);
    // The checkpoint client used a composite SSE id. It cannot be mapped to
    // the phone_changes sequence, so request the initial invalidation and let
    // REST reconciliation establish the canonical cursor.
    if raw.as_deref().is_some_and(|value| value.contains('|')) {
        return Ok(None);
    }
    crate::realtime::parse_cursor(raw.as_deref())
        .map_err(|error| ApiError::validation(error.to_string()))
}

/// Opens a bounded SSE connection for the iOS foreground inbox.
///
/// SSE is a notification-only transport. The durable `phone_changes` cursor
/// is the source of truth and the client reconciles complete data through REST
/// or `/v1/phone/sync` after each invalidation.
async fn phone_events(req: &Request, env: &Env, db: D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, &db).await?;
    let parsed_cursor = parse_event_cursor(req)?;
    let initial_sync = parsed_cursor.is_none();
    let cursor = parsed_cursor.unwrap_or(0);

    let state = SessionStreamState {
        db: Rc::new(db),
        user_id: user.user_id,
        cursor,
        initial_sync,
        poll_count: 0,
        close_after_emit: false,
    };

    let stream = futures_util::stream::try_unfold(state, |mut state| async move {
        if state.close_after_emit {
            return Ok::<Option<(Vec<u8>, SessionStreamState)>, worker::Error>(None);
        }
        if state.initial_sync {
            state.initial_sync = false;
            let data = json!({
                "id": state.cursor.to_string(),
                "type": "sync.required",
                "session_id": Value::Null,
                "version": 0,
            });
            let chunk =
                crate::realtime::sse_frame(&state.cursor.to_string(), "sync.required", &data)
                    .map_err(|error| {
                        stream_error(ApiError::new(500, "sse_error", error.to_string()))
                    })?;
            return Ok(Some((chunk.into_bytes(), state)));
        }

        Delay::from(Duration::from_secs(5)).await;
        let rows: Vec<models::PhoneChangeRow> = db::all(
            &state.db,
            "SELECT cursor, user_id, entity_type, entity_id, session_id, version, created_at, deleted_at FROM phone_changes WHERE user_id = ? AND cursor > ? ORDER BY cursor ASC LIMIT 100",
            vec![
                db::text(&state.user_id),
                db::number(state.cursor),
            ],
        )
        .await
        .map_err(stream_error)?;

        let mut chunk = String::new();
        for row in rows {
            state.cursor = row.cursor;
            // SSE is an invalidation hint only. Keep the payload small and
            // force clients to read the authoritative snapshot through REST
            // or /v1/phone/sync.
            let event_type = phone_change_event_type(&row.entity_type);
            let data = json!({
                "id": row.cursor.to_string(),
                "type": event_type,
                "session_id": row.session_id,
                "version": row.version,
            });
            let frame = crate::realtime::sse_frame(&row.cursor.to_string(), event_type, &data)
                .map_err(|error| {
                    stream_error(ApiError::new(500, "sse_error", error.to_string()))
                })?;
            chunk.push_str(&frame);
        }
        if chunk.is_empty() {
            chunk.push_str(": keep-alive\n\n");
        }

        state.poll_count += 1;
        state.close_after_emit = state.poll_count >= 6;
        Ok(Some((chunk.into_bytes(), state)))
    });

    Ok(Response::builder()
        .with_header("Content-Type", "text/event-stream; charset=utf-8")?
        .with_header("Cache-Control", "no-cache, no-transform")?
        .with_header("Connection", "keep-alive")?
        .with_header("X-Accel-Buffering", "no")?
        .from_stream(stream)?)
}

async fn phone_history(
    req: &Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let exists = sessions::get_session(db, session_id)
        .await?
        .filter(|row| row.user_id == user.user_id && row.deleted_at.is_none())
        .is_some();
    if !exists {
        return Err(ApiError::not_found("Session not found"));
    }
    let before = query_value(req, "before")?;
    json_response(
        audit::list_audit_for_session(
            db,
            &user.user_id,
            session_id,
            before.as_deref(),
            query_limit(req, 100)?,
        )
        .await?,
        200,
    )
}

async fn register_device(req: &mut Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body: DeviceRequest = read_json(req).await?;
    if body.platform.trim().is_empty() {
        return Err(ApiError::validation("platform is required"));
    }
    let existing = if let Some(device_id) = body.device_id.as_deref() {
        db::first::<IdOnly>(
            db,
            "SELECT id FROM devices WHERE user_id = ? AND device_id = ? ORDER BY updated_at DESC LIMIT 1",
            vec![db::text(&user.user_id), db::text(device_id)],
        )
        .await?
    } else {
        db::first::<IdOnly>(
            db,
            "SELECT id FROM devices WHERE user_id = ? AND platform = ? ORDER BY updated_at DESC LIMIT 1",
            vec![db::text(&user.user_id), db::text(&body.platform)],
        )
        .await?
    };
    let now = db::now_iso();
    let device_id = if let Some(existing) = existing {
        db::run(
            db,
            "UPDATE devices SET device_id = COALESCE(?, device_id), push_token = ?, locale = ?, timezone = ?, updated_at = ? WHERE id = ?",
            vec![
                db::optional_text(body.device_id.as_deref()),
                db::optional_text(body.push_token.as_deref()),
                db::optional_text(body.locale.as_deref()),
                db::optional_text(body.timezone.as_deref()),
                db::text(&now),
                db::text(&existing._id),
            ],
        )
        .await?;
        existing._id
    } else {
        let id = new_id("dev")?;
        db::run(
            db,
            "INSERT INTO devices (id, user_id, platform, device_id, push_token, locale, timezone, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            vec![
                db::text(&id),
                db::text(&user.user_id),
                db::text(&body.platform),
                db::optional_text(body.device_id.as_deref()),
                db::optional_text(body.push_token.as_deref()),
                db::optional_text(body.locale.as_deref()),
                db::optional_text(body.timezone.as_deref()),
                db::text(&now),
                db::text(&now),
            ],
        )
        .await?;
        id
    };
    json_response(
        json!({
            "device_id": device_id,
            "platform": body.platform,
            "push_token_registered": body.push_token.is_some(),
            "locale": body.locale,
            "timezone": body.timezone,
        }),
        200,
    )
}

async fn phone_reply(
    req: &mut Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body: PhoneReplyRequest = read_json(req).await?;
    if body.action_key.trim().is_empty() {
        return Err(ApiError::validation("action_key is required"));
    }
    let idempotency_key = body.idempotency_key.as_deref();
    let request_hash = sha256_hex(
        &json!({
            "session_id": session_id,
            "action_key": body.action_key.trim(),
            "utterance": body.utterance.as_deref(),
        })
        .to_string(),
    );
    let operation = phone_operations::begin(
        db,
        &user.user_id,
        "reply",
        idempotency_key,
        &request_hash,
        session_id,
        None,
    )
    .await?;
    if let Some(replayed) = operation.replay {
        return json_response(replayed, 200);
    }
    let result = sessions::phone_reply(
        db,
        &user.user_id,
        session_id,
        body.action_key.trim(),
        body.utterance.as_deref(),
    )
    .await;
    match result {
        Ok(value) => {
            phone_operations::complete(
                db,
                &user.user_id,
                "reply",
                idempotency_key,
                &request_hash,
                session_id,
                operation.claim_token.as_deref(),
                &value,
            )
            .await?;
            json_response(value, 200)
        }
        Err(error) => {
            phone_operations::release(
                db,
                &user.user_id,
                "reply",
                idempotency_key,
                operation.claim_token.as_deref(),
            )
            .await;
            Err(error)
        }
    }
}

async fn phone_confirm(
    req: &mut Request,
    env: &Env,
    db: &D1Database,
    session_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body: PhoneConfirmRequest = read_json(req).await?;
    if body.action_id.trim().is_empty() {
        return Err(ApiError::validation("action_id is required"));
    }
    let idempotency_key = body.idempotency_key.as_deref();
    let request_hash = sha256_hex(
        &json!({
            "session_id": session_id,
            "action_id": body.action_id.trim(),
            "confirm": body.confirm,
        })
        .to_string(),
    );
    let operation = phone_operations::begin(
        db,
        &user.user_id,
        "confirm",
        idempotency_key,
        &request_hash,
        session_id,
        Some(body.action_id.trim()),
    )
    .await?;
    if let Some(replayed) = operation.replay {
        return json_response(replayed, 200);
    }
    let result = sessions::phone_confirm(
        db,
        &user.user_id,
        session_id,
        body.action_id.trim(),
        body.confirm,
    )
    .await;
    match result {
        Ok(value) => {
            phone_operations::complete(
                db,
                &user.user_id,
                "confirm",
                idempotency_key,
                &request_hash,
                session_id,
                operation.claim_token.as_deref(),
                &value,
            )
            .await?;
            json_response(value, 200)
        }
        Err(error) => {
            phone_operations::release(
                db,
                &user.user_id,
                "confirm",
                idempotency_key,
                operation.claim_token.as_deref(),
            )
            .await;
            Err(error)
        }
    }
}

async fn phone_create_command(
    req: &mut Request,
    env: &Env,
    db: &D1Database,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body: CommandEnvelope = read_json(req).await?;
    json_response(crate::commands::create(db, &user.user_id, body).await?, 202)
}

async fn phone_get_command(
    req: &Request,
    env: &Env,
    db: &D1Database,
    command_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let command = crate::commands::get_for_user(db, &user.user_id, command_id)
        .await?
        .ok_or_else(|| ApiError::not_found("Command not found"))?;
    json_response(crate::commands::response(&command, None), 200)
}

async fn phone_list_commands(req: &Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let before = query_value(req, "before")?;
    let state = query_value(req, "state")?;
    let session_id = query_value(req, "session_id")?;
    json_response(
        crate::commands::list_for_user(
            db,
            &user.user_id,
            before.as_deref(),
            state.as_deref(),
            session_id.as_deref(),
            query_limit(req, 50)?,
        )
        .await?,
        200,
    )
}

async fn phone_confirm_command(
    req: &mut Request,
    env: &Env,
    db: &D1Database,
    command_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    let body: ConfirmationRequest = read_json(req).await?;
    json_response(
        crate::commands::confirm(db, &user.user_id, command_id, &body.confirmation_token).await?,
        200,
    )
}

async fn phone_cancel_command(
    req: &Request,
    env: &Env,
    db: &D1Database,
    command_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    json_response(
        crate::commands::cancel(db, &user.user_id, command_id).await?,
        200,
    )
}

async fn phone_undo_command(
    req: &Request,
    env: &Env,
    db: &D1Database,
    command_id: &str,
) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    json_response(
        crate::commands::undo(env, db, &user.user_id, command_id, providers::load(env)?).await?,
        202,
    )
}

async fn dev_pushes(req: &Request, env: &Env, db: &D1Database) -> ApiResult<Response> {
    let user = require_user(req, env, db).await?;
    json_response(
        json!({
            "pushes": push::list_pushes(db, &user.user_id, query_limit(req, 50)?).await?,
        }),
        200,
    )
}

fn add_common_headers(
    mut response: Response,
    env: &Env,
    _request_origin: Option<String>,
) -> Result<Response> {
    let production = config_value(env, "NODE_ENV", "development")
        .trim()
        .eq_ignore_ascii_case("production");
    let configured_origin = config_value(env, "CORS_ORIGIN", if production { "" } else { "*" });
    if !configured_origin.trim().is_empty() {
        response
            .headers_mut()
            .set("Access-Control-Allow-Origin", &configured_origin)?;
    }
    response.headers_mut().set(
        "Access-Control-Allow-Headers",
        "Authorization, Content-Type, X-Agent-Key, X-Device-ID, X-Request-ID",
    )?;
    response.headers_mut().set(
        "Access-Control-Allow-Methods",
        "GET, POST, PATCH, DELETE, OPTIONS",
    )?;
    response
        .headers_mut()
        .set("Access-Control-Max-Age", "86400")?;
    response
        .headers_mut()
        .set("X-Content-Type-Options", "nosniff")?;
    response
        .headers_mut()
        .set("Referrer-Policy", "no-referrer")?;
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::validate_model_manifest;
    use serde_json::json;

    #[test]
    fn model_manifest_requires_integrity_and_capability_fields() {
        let valid = json!({
            "schema_version": 1,
            "model_id": "whisperkit-base",
            "model_version": "1.0.0",
            "sha256": "a".repeat(64),
            "signature": "sig-ed25519",
            "size_bytes": 1024,
            "minimum_capability": "iphone13"
        });
        assert!(validate_model_manifest(&valid, "whisperkit-base").is_ok());

        let mut invalid = valid.clone();
        invalid["sha256"] = json!("not-a-hash");
        assert!(validate_model_manifest(&invalid, "whisperkit-base").is_err());
    }
}
