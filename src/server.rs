use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::thread;
use tiny_http::{Header, Method, Request, Response, Server};
use uuid::Uuid;

use crate::app::App;
use crate::model::{AccountView, RunningCodexProcess};
use crate::secrets::SecretStore;

const HTML_CONTENT: &str = include_str!("dashboard.html");
const MENU_HTML_CONTENT: &str = include_str!("menu.html");

/// Concurrent workers for the local dashboard HTTP server.
fn http_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().clamp(2, 8))
        .unwrap_or(4)
}

#[derive(Serialize)]
struct SettingsView {
    show_quota_in_menu_bar: bool,
    auto_switch_on_limit: bool,
    disable_blocker_warnings: bool,
    ui_language: String,
    ui_language_resolved: String,
}

#[derive(Serialize)]
struct FullStatusPayload {
    environment: crate::model::EnvironmentKind,
    active_codex_id: Option<Uuid>,
    active_cursor_id: Option<Uuid>,
    active_claude_id: Option<Uuid>,
    live_codex: Option<crate::model::DisplayIdentity>,
    live_cursor: Option<crate::model::DisplayIdentity>,
    live_claude: Option<crate::model::DisplayIdentity>,
    /// True while a Codex add-account / workspace login session is in progress.
    codex_add_account_pending: bool,
    accounts: Vec<AccountView>,
    process_warnings: Vec<RunningCodexProcess>,
    settings: SettingsView,
    logs: Vec<crate::activity::ActivityEntryView>,
}

#[derive(Deserialize)]
struct ActivateRequest {
    id: String,
}

#[derive(Deserialize)]
struct SaveRequest {
    app: String,
}

#[derive(Deserialize)]
struct AddAccountRequest {
    /// `begin` | `finish` | `cancel`
    action: String,
    /// `codex` (default), `cursor`, or `claude`
    app: Option<String>,
}

#[derive(Deserialize)]
struct ImportCookiesRequest {
    provider: Option<String>,
    json: String,
    label: Option<String>,
}

#[derive(Deserialize)]
struct ArchiveRequest {
    id: Uuid,
    archived: bool,
}

#[derive(Deserialize)]
struct DeleteRequest {
    id: Uuid,
}

#[derive(Deserialize)]
struct SettingsRequest {
    key: String,
    value: serde_json::Value,
}

pub fn start_dashboard_server<S>(app: Arc<App<S>>) -> Result<u16>
where
    S: SecretStore + Send + Sync + 'static,
{
    let port = 5032;
    let server = match Server::http(format!("127.0.0.1:{port}")) {
        Ok(s) => s,
        Err(_) => {
            // Fallback to random port
            Server::http("127.0.0.1:0")
                .map_err(|e| anyhow::anyhow!("failed to bind HTTP server to any port: {e}"))?
        }
    };

    let bound_port = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr.port(),
        _ => 5032,
    };

    thread::spawn(move || {
        serve(server, app);
    });

    Ok(bound_port)
}

fn serve<S>(server: Server, app: Arc<App<S>>)
where
    S: SecretStore + Send + Sync + 'static,
{
    let server = Arc::new(server);
    let workers = http_worker_count();
    let mut joins = Vec::with_capacity(workers);
    for i in 0..workers {
        let server = Arc::clone(&server);
        let app = Arc::clone(&app);
        let handle = thread::Builder::new()
            .name(format!("dashboard-http-{i}"))
            .spawn(move || {
                for request in server.incoming_requests() {
                    handle_request(request, &app);
                }
            })
            .expect("failed to spawn dashboard HTTP worker");
        joins.push(handle);
    }
    for handle in joins {
        let _ = handle.join();
    }
}

fn path_and_query(url: &str) -> (&str, &str) {
    url.split_once('?').unwrap_or((url, ""))
}

fn query_flag(query: &str, names: &[&str]) -> bool {
    for part in query.split('&') {
        let (key, value) = part.split_once('=').unwrap_or((part, "1"));
        if !names.iter().any(|n| key.eq_ignore_ascii_case(n)) {
            continue;
        }
        let v = value.trim();
        if v.is_empty()
            || v == "1"
            || v.eq_ignore_ascii_case("true")
            || v.eq_ignore_ascii_case("yes")
        {
            return true;
        }
    }
    false
}

fn json_response(body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap())
}

fn html_response(body: &'static str) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body)
        .with_header(
            Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap(),
        )
        .with_header(Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap())
}

fn handle_request<S>(mut request: Request, app: &App<S>)
where
    S: SecretStore + Send + Sync + 'static,
{
    let url = request.url().to_owned();
    let (path, query) = path_and_query(&url);
    let method = request.method().clone();

    let response = match (&method, path) {
        (&Method::Get, "/") => html_response(HTML_CONTENT),
        (&Method::Get, "/menu") => html_response(MENU_HTML_CONTENT),
        (&Method::Get, "/api/status") => {
            // Process scan is expensive — only when Overview asks (`?processes=1`).
            let include_processes = query_flag(query, &["processes", "include_processes"]);
            match get_full_status(app, include_processes) {
                Ok(payload) => {
                    let json = serde_json::to_string(&payload).unwrap_or_else(|_| {
                        r#"{"success":false,"error":"encode failed"}"#.to_owned()
                    });
                    json_response(json)
                }
                Err(error) => {
                    let err_json =
                        serde_json::json!({ "success": false, "error": format!("{error:#}") })
                            .to_string();
                    json_response(err_json).with_status_code(500)
                }
            }
        }
        (&Method::Post, "/api/activate") => {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);

            let res = if let Ok(req) = serde_json::from_str::<ActivateRequest>(&body) {
                if req.id == "best" {
                    match app.pick_best_account(true, true) {
                        Ok(output) => {
                            if output.switched {
                                serde_json::json!({ "success": true })
                            } else {
                                serde_json::json!({ "success": false, "error": "No better account found" })
                            }
                        }
                        Err(e) => {
                            serde_json::json!({ "success": false, "error": format!("{e:#}") })
                        }
                    }
                } else if let Ok(uuid) = Uuid::parse_str(&req.id) {
                    match app.activate(uuid) {
                        Ok(_) => serde_json::json!({ "success": true }),
                        Err(e) => {
                            serde_json::json!({ "success": false, "error": format!("{e:#}") })
                        }
                    }
                } else {
                    serde_json::json!({ "success": false, "error": "Invalid account ID" })
                }
            } else {
                serde_json::json!({ "success": false, "error": "Invalid request body" })
            };

            json_response(res.to_string())
        }
        (&Method::Post, "/api/save") => {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);

            let res = if let Ok(req) = serde_json::from_str::<SaveRequest>(&body) {
                let save_res = match req.app.to_lowercase().as_str() {
                    "cursor" => app.save_cursor_current().map(|_| ()),
                    "claude" => app.save_claude_current().map(|_| ()),
                    _ => app.save_current().map(|_| ()),
                };
                match save_res {
                    Ok(_) => serde_json::json!({ "success": true }),
                    Err(e) => {
                        serde_json::json!({ "success": false, "error": format!("{e:#}") })
                    }
                }
            } else {
                serde_json::json!({ "success": false, "error": "Invalid request body" })
            };

            json_response(res.to_string())
        }
        (&Method::Post, "/api/add-account") => {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let res = match serde_json::from_str::<AddAccountRequest>(&body) {
                Ok(req) => handle_add_account(app, &req),
                Err(_) => {
                    serde_json::json!({ "success": false, "error": "Invalid request body" })
                }
            };
            json_response(res.to_string())
        }
        (&Method::Post, "/api/import-cookies") => {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            // Never log request body — may contain cookies/tokens.
            let res = if let Ok(req) = serde_json::from_str::<ImportCookiesRequest>(&body) {
                let provider = req.provider.as_deref().unwrap_or("codex");
                match app.import_cookies_json(provider, &req.json, req.label) {
                    Ok(output) => serde_json::json!({
                        "success": true,
                        "account_id": output.account_id,
                        "email": output.email,
                        "created": output.created,
                        "warnings": output.warnings,
                    }),
                    Err(e) => {
                        serde_json::json!({ "success": false, "error": format!("{e:#}") })
                    }
                }
            } else {
                serde_json::json!({ "success": false, "error": "Invalid request body" })
            };

            json_response(res.to_string())
        }
        (&Method::Post, "/api/archive") => {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);

            let res = if let Ok(req) = serde_json::from_str::<ArchiveRequest>(&body) {
                match app.set_account_archived(req.id, req.archived) {
                    Ok(_) => serde_json::json!({ "success": true }),
                    Err(e) => {
                        serde_json::json!({ "success": false, "error": format!("{e:#}") })
                    }
                }
            } else {
                serde_json::json!({ "success": false, "error": "Invalid request body" })
            };

            json_response(res.to_string())
        }
        (&Method::Post, "/api/delete") => {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);

            let res = if let Ok(req) = serde_json::from_str::<DeleteRequest>(&body) {
                match app.delete(req.id) {
                    Ok(_) => serde_json::json!({ "success": true }),
                    Err(e) => {
                        serde_json::json!({ "success": false, "error": format!("{e:#}") })
                    }
                }
            } else {
                serde_json::json!({ "success": false, "error": "Invalid request body" })
            };

            json_response(res.to_string())
        }
        (&Method::Post, "/api/kill") => {
            crate::process::force_quit_all_switch_blocking_processes();
            json_response(serde_json::json!({ "success": true }).to_string())
        }
        (&Method::Post, "/api/settings") => {
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);

            let res = if let Ok(req) = serde_json::from_str::<SettingsRequest>(&body) {
                let set_res = match req.key.as_str() {
                    "menu-bar" => req
                        .value
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("menu-bar expects a boolean"))
                        .and_then(|value| app.set_show_quota_in_menu_bar(value).map(|_| ())),
                    "auto-switch" => req
                        .value
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("auto-switch expects a boolean"))
                        .and_then(|value| app.set_auto_switch_on_limit(value).map(|_| ())),
                    "disable-warnings" => req
                        .value
                        .as_bool()
                        .ok_or_else(|| anyhow::anyhow!("disable-warnings expects a boolean"))
                        .and_then(|value| app.set_disable_blocker_warnings(value).map(|_| ())),
                    "ui-language" => req
                        .value
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("ui-language expects a string"))
                        .and_then(|value| app.set_ui_language(value).map(|_| ())),
                    _ => Err(anyhow::anyhow!("unknown setting key")),
                };
                match set_res {
                    Ok(_) => serde_json::json!({ "success": true }),
                    Err(e) => {
                        serde_json::json!({ "success": false, "error": format!("{e:#}") })
                    }
                }
            } else {
                serde_json::json!({ "success": false, "error": "Invalid request body" })
            };

            json_response(res.to_string())
        }
        _ => {
            json_response(serde_json::json!({ "success": false, "error": "not found" }).to_string())
                .with_status_code(404)
        }
    };

    let _ = request.respond(response);
}

fn get_full_status<S>(app: &App<S>, include_processes: bool) -> Result<FullStatusPayload>
where
    S: SecretStore,
{
    let status = app
        .status_with_processes(include_processes)
        .context("failed to fetch Codex status")?;
    let cursor_status = app.cursor_status_with_processes(include_processes).ok();
    let claude_status = app.claude_status_with_processes(include_processes).ok();
    let list = app.list().context("failed to fetch accounts list")?;

    let mut process_warnings = Vec::new();
    if include_processes {
        // Warm shared cache once, then merge — nested status_* already hit the same cache.
        let processes = crate::process::detect_all_processes();
        process_warnings.extend(processes.codex);
        process_warnings.extend(processes.cursor);
        process_warnings.extend(processes.claude);
        if process_warnings.is_empty() {
            process_warnings.extend(status.process_warnings.clone());
            if let Some(c_status) = &cursor_status {
                process_warnings.extend(c_status.process_warnings.clone());
            }
            if let Some(cl_status) = &claude_status {
                process_warnings.extend(cl_status.process_warnings.clone());
            }
        }
        process_warnings.sort_by_key(|w| w.pid);
        process_warnings.dedup_by_key(|w| w.pid);
    }

    // One settings disk read for all toggles.
    let settings = crate::settings::load_settings(&app.env().app_data_dir).unwrap_or_default();
    let resolved_lang = crate::settings::resolve_ui_language(&settings.ui_language);

    let logs = crate::activity::read_activity_log(&app.env().app_data_dir).unwrap_or_default();

    Ok(FullStatusPayload {
        environment: status.environment,
        active_codex_id: status.current_account_saved_id,
        active_cursor_id: cursor_status
            .as_ref()
            .and_then(|s| s.current_account_saved_id),
        active_claude_id: claude_status
            .as_ref()
            .and_then(|s| s.current_account_saved_id),
        live_codex: status.current_account.clone(),
        live_cursor: cursor_status.and_then(|s| s.current_account),
        live_claude: claude_status.and_then(|s| s.current_account),
        codex_add_account_pending: crate::codex::add_account_session_active(app.env()),
        accounts: list.accounts,
        process_warnings,
        settings: SettingsView {
            show_quota_in_menu_bar: settings.show_quota_in_menu_bar,
            auto_switch_on_limit: settings.auto_switch_on_limit,
            disable_blocker_warnings: settings.disable_blocker_warnings,
            ui_language: settings.ui_language,
            ui_language_resolved: resolved_lang.as_str().to_owned(),
        },
        logs,
    })
}

fn handle_add_account<S>(app: &App<S>, req: &AddAccountRequest) -> serde_json::Value
where
    S: SecretStore + Send + Sync + 'static,
{
    let provider = req
        .app
        .as_deref()
        .unwrap_or("codex")
        .trim()
        .to_ascii_lowercase();
    let action = req.action.trim().to_ascii_lowercase();

    match (provider.as_str(), action.as_str()) {
        ("cursor", "begin") | ("claude", "begin") => serde_json::json!({
            "success": true,
            "mode": "save_live",
            "app": provider,
            "message": if provider == "cursor" {
                "Sign in to Cursor with the new account, then use Save signed-in session."
            } else {
                "Sign in to Claude with the new account, then use Save signed-in session."
            },
        }),
        ("cursor", _) | ("claude", _) => serde_json::json!({
            "success": false,
            "error": "Use Save signed-in session for Cursor/Claude (cookie import is not supported)."
        }),
        ("codex", "begin") => {
            let begin = if crate::codex::add_account_session_active(app.env()) {
                // Resume stuck session instead of erroring.
                Ok(())
            } else {
                app.begin_add_account_session()
            };
            match begin {
                Ok(()) => {
                    let codex_home = app.env().codex_root.clone();
                    thread::spawn(move || {
                        let launch =
                            crate::process::relaunch_codex_for_interactive_login(&codex_home);
                        eprintln!(
                            "add-account login method={} oauth_ready={} detail={}",
                            launch.method, launch.oauth_port_ready, launch.detail
                        );
                    });
                    serde_json::json!({
                        "success": true,
                        "mode": "codex_login",
                        "pending": true,
                        "message": "Browser login is started via `codex login` (not blank Desktop OAuth). Close any empty auth.openai.com tabs, finish the new browser window, then choose Finish."
                    })
                }
                Err(e) => serde_json::json!({ "success": false, "error": format!("{e:#}") }),
            }
        }
        ("codex", "finish") => match app.save_during_add_account_session() {
            Ok(output) => {
                // Quit → restore → relaunch is slow (osascript/pkill). Do it off the
                // HTTP worker so other workers can keep serving /api/status.
                let env = app.env().clone();
                thread::spawn(move || {
                    crate::process::quit_running_codex_app();
                    if let Err(error) = crate::codex::restore_add_account_backup(&env) {
                        eprintln!("add-account restore failed: {error:#}");
                    }
                    crate::process::launch_codex_app();
                });
                serde_json::json!({
                    "success": true,
                    "pending": false,
                    "email": output.account.email,
                    "account_id": output.account.id,
                    "message": format!("Added {}", output.account.email),
                })
            }
            Err(e) => serde_json::json!({ "success": false, "error": format!("{e:#}") }),
        },
        ("codex", "cancel") => match app.cancel_add_account_session() {
            Ok(()) => {
                thread::spawn(|| {
                    crate::process::quit_running_codex_app();
                    crate::process::launch_codex_app();
                });
                serde_json::json!({
                    "success": true,
                    "pending": false,
                    "message": "Cancelled. Original account restored."
                })
            }
            Err(e) => serde_json::json!({ "success": false, "error": format!("{e:#}") }),
        },
        _ => serde_json::json!({
            "success": false,
            "error": format!("Unknown add-account action '{action}' for '{provider}'")
        }),
    }
}
