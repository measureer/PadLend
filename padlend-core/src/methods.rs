//! RPC 方法表。方法名与 Python `service/api.py` 的 25 个一一对应，
//! 顺序不重要，但**名字不能改**（前端与 golden trace 都按名字索引）。
//!
//! 实现进度用 `tools/rpc_trace.py replay` 度量：每接一个方法，25/25 的分子 +1。

use serde_json::{json, Value};

use crate::{config, constants};

/// 与 `service/api.py` 完全一致的 25 个方法名。
pub const METHODS: [&str; 25] = [
    "app.bootstrap",
    "app.shutdown",
    "config.get",
    "config.save",
    "devices.add",
    "devices.connect",
    "devices.disconnect",
    "devices.locate",
    "devices.remove",
    "devices.scan",
    "diagnostics.export",
    "diagnostics.run",
    "diagnostics.static",
    "engine.inject_clipboard",
    "engine.metrics",
    "engine.panic",
    "engine.reconnect",
    "engine.state",
    "engine.toggle",
    "layout.desktop",
    "layout.initial",
    "layout.save",
    "logs.recent",
    "system.paths",
    "wizard.check",
];

pub enum Reply {
    Ok(Value),
    Error(String),
    /// 响应先写出去，然后退出读循环。
    Shutdown(Value),
}

pub fn dispatch(method: &str, params: &Value) -> Reply {
    let result = match method {
        "app.shutdown" => return Reply::Shutdown(json!({ "ok": true })),
        "system.paths" => system_paths(),
        "config.get" => config_get(),
        "config.save" => config_save(params),
        _ => return Reply::Error(format!("not implemented: {method}")),
    };
    match result {
        Ok(value) => Reply::Ok(value),
        Err(err) => Reply::Error(err),
    }
}

/// `system.paths`：与 Python `service/api.py:system_paths` 的 6 个键逐字一致。
fn system_paths() -> Result<Value, String> {
    let config_path = constants::config_path().map_err(|err| err.to_string())?;
    let log_dir = constants::log_dir().map_err(|err| err.to_string())?;
    let notices = constants::app_dir().join("THIRD_PARTY_NOTICES.md");
    Ok(json!({
        "configPath": config_path.display().to_string(),
        "logDir": log_dir.display().to_string(),
        "noticesPath": notices.display().to_string(),
        "version": constants::VERSION,
        "license": constants::LICENSE,
        "appName": constants::APP_NAME,
    }))
}

/// `config.get`：`{"config", "issues", "path"}`（见 `service/api.py:config_get`）。
fn config_get() -> Result<Value, String> {
    let path = constants::config_path().map_err(|err| err.to_string())?;
    let cfg = config::load_config(Some(&path)).map_err(|err| err.to_string())?;
    let dict = cfg.to_dict().map_err(|err| err.to_string())?;
    Ok(json!({
        "config": dict,
        "issues": cfg.issues(),
        "path": path.display().to_string(),
    }))
}

/// `config.save`：把补丁深合并进当前配置树 → 重新校验 → 原子写回。
///
/// Python 的 `apply_config_patch` 是逐字段强转 + 失败回滚；这里是"合并到 YAML 树再
/// 走 `from_mapping`（带修复）"，对 GUI 的整段提交等价，但**没有**逐字段错误文本，
/// 差异登记在 `docs/B2_MIGRATION_PLAN.md` 的已知偏差里。
fn config_save(params: &Value) -> Result<Value, String> {
    let path = constants::config_path().map_err(|err| err.to_string())?;
    let patch = params
        .get("patch")
        .cloned()
        .unwrap_or_else(|| Value::Object(Default::default()));
    if !patch.is_object() {
        return Err("配置补丁必须是对象".to_string());
    }

    let current = config::Config::load(&path);
    let mut tree: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&current.to_yaml().map_err(|err| err.to_string())?)
            .map_err(|err| format!("当前配置无法解析：{err}"))?;
    merge_yaml(&mut tree, &json_to_yaml(&patch));
    let mapping = tree.as_mapping().ok_or("配置根必须是映射")?;
    let cfg = config::Config::from_mapping(mapping);

    let problems = cfg.validate();
    if !problems.is_empty() {
        return Err(problems.join("；"));
    }
    cfg.save(&path).map_err(|err| err.to_string())?;
    Ok(json!({
        "config": cfg.to_dict().map_err(|err| err.to_string())?,
        "issues": [],
        "path": path.display().to_string(),
    }))
}

/// serde_json 值 → YAML 值（补丁合并用）。
fn json_to_yaml(value: &Value) -> serde_yaml_ng::Value {
    use serde_yaml_ng::Value as Yaml;
    match value {
        Value::Null => Yaml::Null,
        Value::Bool(flag) => Yaml::Bool(*flag),
        Value::Number(number) => {
            if let Some(int) = number.as_i64() {
                Yaml::Number(int.into())
            } else if let Some(uint) = number.as_u64() {
                Yaml::Number(uint.into())
            } else {
                Yaml::Number(number.as_f64().unwrap_or(0.0).into())
            }
        }
        Value::String(text) => Yaml::String(text.clone()),
        Value::Array(items) => Yaml::Sequence(items.iter().map(json_to_yaml).collect()),
        Value::Object(map) => {
            let mut mapping = serde_yaml_ng::Mapping::new();
            for (key, item) in map {
                mapping.insert(Yaml::String(key.clone()), json_to_yaml(item));
            }
            Yaml::Mapping(mapping)
        }
    }
}

/// 递归深合并：映射逐键合并，其余类型直接覆盖。
fn merge_yaml(base: &mut serde_yaml_ng::Value, patch: &serde_yaml_ng::Value) {
    use serde_yaml_ng::Value as Yaml;
    if let (Yaml::Mapping(target), Yaml::Mapping(source)) = (&mut *base, patch) {
        for (key, value) in source {
            match target.get_mut(key) {
                Some(slot) => merge_yaml(slot, value),
                None => {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        return;
    }
    *base = patch.clone();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_table_matches_python_contract() {
        assert_eq!(METHODS.len(), 25);
        assert!(METHODS.contains(&"system.paths"));
        assert!(METHODS.contains(&"wizard.check"));
    }

    #[test]
    fn system_paths_has_the_six_contract_keys() {
        let value = system_paths().expect("system_paths 失败");
        let object = value.as_object().expect("应为对象");
        let mut keys: Vec<&String> = object.keys().collect();
        keys.sort();
        assert_eq!(
            keys,
            vec!["appName", "configPath", "license", "logDir", "noticesPath", "version"]
        );
        assert_eq!(object["appName"], "PadLend");
        assert_eq!(object["version"], constants::VERSION);
    }

    #[test]
    fn shutdown_returns_ok_object() {
        match dispatch("app.shutdown", &Value::Null) {
            Reply::Shutdown(value) => assert_eq!(value, json!({ "ok": true })),
            _ => panic!("app.shutdown 必须返回 Shutdown"),
        }
    }

    #[test]
    fn unknown_method_is_reported_as_not_implemented() {
        match dispatch("devices.scan", &Value::Null) {
            Reply::Error(err) => assert!(err.starts_with("not implemented:")),
            _ => panic!("未实现的方法必须回 not implemented"),
        }
    }

    #[test]
    fn json_to_yaml_keeps_scalar_types() {
        let yaml = json_to_yaml(&json!({"a": 1, "b": 2.5, "c": "x", "d": true, "e": null}));
        let mapping = yaml.as_mapping().unwrap();
        assert_eq!(mapping.get("a").unwrap().as_i64(), Some(1));
        assert_eq!(mapping.get("b").unwrap().as_f64(), Some(2.5));
        assert_eq!(mapping.get("c").unwrap().as_str(), Some("x"));
        assert_eq!(mapping.get("d").unwrap().as_bool(), Some(true));
        assert!(mapping.get("e").unwrap().is_null());
    }

    #[test]
    fn merge_yaml_deep_merges_and_keeps_untouched_keys() {
        let mut base: serde_yaml_ng::Value =
            serde_yaml_ng::from_str("device:\n  serial: A\nmisc:\n  forward_audio: true\n").unwrap();
        let patch = json_to_yaml(&json!({"device": {"serial": "B"}}));
        merge_yaml(&mut base, &patch);
        let mapping = base.as_mapping().unwrap();
        let device = mapping.get("device").unwrap().as_mapping().unwrap();
        assert_eq!(device.get("serial").unwrap().as_str(), Some("B"));
        let misc = mapping.get("misc").unwrap().as_mapping().unwrap();
        assert_eq!(misc.get("forward_audio").unwrap().as_bool(), Some(true));
    }
}
