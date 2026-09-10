//! WebUI 的外观偏好，存在 daemon 这边。
//!
//! `localStorage` 按 **origin** 隔离：`http://127.0.0.1:8300` 和
//! `http://192.168.1.7:8300` 是两个源，同一台 daemon 换个地址进来就是另一份主题
//! 设置——这就是「从不同 IP 进 WebUI 主题不一样」的全部原因。
//!
//! 外观是「这台 Miyu 长什么样」，不是「这个浏览器长什么样」，所以落盘在服务端。
//! 只收白名单里那几个键、只收短字符串：这里不是给前端当通用键值仓库用的，
//! 窗口尺寸相关的偏好（侧栏折叠、分栏比例）仍然留在浏览器本地，手机和台式机
//! 本来就该不一样。

use crate::web::*;

/// 跟着 daemon 走的外观键。改这里就是在改「哪些设置跨设备同步」。
const SYNCED_KEYS: [&str; 6] = [
    "theme",
    "colorScheme",
    "chatFontSize",
    "reasoningExpanded",
    "toolExpanded",
    "procCollapse",
];

const MAX_VALUE_LEN: usize = 64;

/// 读改写要互斥：两个标签页同时切主题会互相覆盖掉对方那半份文件。
static WRITE_LOCK: Mutex<()> = Mutex::new(());

fn prefs_file(paths: &MiyuPaths) -> PathBuf {
    paths.state_dir.join("web-ui.json")
}

fn load(paths: &MiyuPaths) -> Map<String, Value> {
    let Ok(raw) = std::fs::read_to_string(prefs_file(paths)) else {
        return Map::new();
    };
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(map)) => map,
        _ => Map::new(),
    }
}

/// 白名单 + 长度过滤。值一律是字符串：前端存的本来就是字符串，
/// 放开类型只会让两边对「true 还是 \"true\"」各有一套理解。
fn sanitize(input: &Map<String, Value>) -> Map<String, Value> {
    let mut clean = Map::new();
    for key in SYNCED_KEYS {
        let Some(Value::String(value)) = input.get(key) else {
            continue;
        };
        if value.chars().count() <= MAX_VALUE_LEN {
            clean.insert(key.to_string(), Value::String(value.clone()));
        }
    }
    clean
}

pub(in crate::web) async fn get_ui_prefs(
    State(state): State<DaemonState>,
    headers: HeaderMap,
) -> std::result::Result<Response, ApiError> {
    require_auth(&headers, &state)?;
    let prefs = sanitize(&load(&state.paths));
    let mut response = Json(Value::Object(prefs)).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

pub(in crate::web) async fn update_ui_prefs(
    State(state): State<DaemonState>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> std::result::Result<Json<Value>, ApiError> {
    require_mutation(&headers, &state)?;
    let Value::Object(incoming) = request else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "expected a JSON object",
        ));
    };
    let incoming = sanitize(&incoming);
    let guard = WRITE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let mut prefs = sanitize(&load(&state.paths));
    // 合并而不是整体替换：前端一次只改一个键，整体替换会让还没读到新值的
    // 另一个标签页把别人的改动抹掉。
    for (key, value) in incoming {
        prefs.insert(key, value);
    }
    let path = prefs_file(&state.paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(ApiError::internal)?;
    }
    let body =
        serde_json::to_string_pretty(&Value::Object(prefs.clone())).map_err(ApiError::internal)?;
    std::fs::write(&path, body).map_err(ApiError::internal)?;
    drop(guard);
    Ok(Json(Value::Object(prefs)))
}
