//! 对应 `service/serializers.py`：RPC 返回值里所有 DTO 的字段名与类型来源。
//!
//! golden trace 只比结构（键集合 / 容器类型 / 标量类型），所以这里必须逐字段对齐
//! Python：少一个键、把整数写成浮点、把 null 写成空对象都会红。

use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::logic::geometry::{Align, Edge, EdgeSegment, MonitorInfo, Placement, Rect};
use crate::logic::router::DeviceFacts;
use crate::tablet::adb::DeviceEntry;
use crate::tablet::locator::Located;

/// `rect_dict`：右上左下 + 宽高，全部整数。
pub fn rect_dict(rect: &Rect) -> Value {
    json!({
        "left": rect.left,
        "top": rect.top,
        "right": rect.right,
        "bottom": rect.bottom,
        "width": rect.width(),
        "height": rect.height(),
    })
}

/// `monitor_dict`：显示器条目。
pub fn monitor_dict(monitor: &MonitorInfo) -> Value {
    json!({
        "handle": monitor.handle,
        "device": monitor.device,
        "name": monitor.name(),
        "rect": rect_dict(&monitor.rect),
        "dpi": monitor.dpi,
        "scale": monitor.scale(),
        "primary": monitor.primary,
    })
}

/// `segment_dict`：外轮廓边段。
pub fn segment_dict(segment: &EdgeSegment) -> Value {
    json!({
        "monitorHandle": segment.monitor_handle,
        "edge": segment.edge.as_str(),
        "spanStart": segment.span_start,
        "spanEnd": segment.span_end,
        "length": segment.length(),
    })
}

/// `placement_dict`：`None` → JSON null（bootstrap 的 `placement` 允许为 null）。
pub fn placement_dict(placement: Option<&Placement>) -> Value {
    let Some(placement) = placement else {
        return Value::Null;
    };
    json!({
        "monitorHandle": placement.segment.monitor_handle,
        "edge": placement.segment.edge.as_str(),
        "spanStart": placement.segment.span_start,
        "spanEnd": placement.segment.span_end,
        "length": placement.segment.length(),
        "offset": placement.offset,
        "align": placement.align.as_str(),
        "cornerPriority": placement.corner_priority.map(|edge| edge.as_str()),
    })
}

/// `placement_from_dict`：GUI 拖拽结果 → `Placement`；非法值按 Python 的回退规则处理。
pub fn placement_from_dict(data: &Value) -> Placement {
    let monitor_handle = data
        .get("monitorHandle")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let edge = data
        .get("edge")
        .and_then(Value::as_str)
        .and_then(|text| Edge::try_from(text).ok())
        .unwrap_or(Edge::Right);
    let span_start = int_field(data, "spanStart");
    let span_end = int_field(data, "spanEnd");
    let align = data
        .get("align")
        .and_then(Value::as_str)
        .and_then(|text| Align::try_from(text).ok())
        .unwrap_or(Align::Proportional);
    let corner_priority = data
        .get("cornerPriority")
        .and_then(Value::as_str)
        .and_then(|text| Edge::try_from(text).ok());
    let offset = data
        .get("offset")
        .and_then(Value::as_f64)
        .unwrap_or(0.5)
        .clamp(0.0, 1.0);
    Placement {
        segment: EdgeSegment {
            monitor_handle,
            edge,
            span_start,
            span_end,
        },
        offset,
        align,
        corner_priority,
    }
}

fn int_field(data: &Value, key: &str) -> i32 {
    let value = data.get(key);
    let number = value.and_then(Value::as_i64).or_else(|| {
        value
            .and_then(Value::as_f64)
            .and_then(|float| if float.is_finite() { Some(float as i64) } else { None })
    });
    match number {
        Some(number) => number.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
        None => 0,
    }
}

/// `facts_dict`：未连接 → null；型号下划线换空格、朝向按宽高优先。
pub fn facts_dict(facts: Option<&DeviceFacts>) -> Value {
    let Some(facts) = facts else {
        return Value::Null;
    };
    let orientation = if facts.width > 0 && facts.height > 0 {
        if facts.width >= facts.height {
            "landscape"
        } else {
            "portrait"
        }
    } else if facts.orientation & 1 == 1 {
        "portrait"
    } else {
        "landscape"
    };
    json!({
        "serial": facts.serial,
        "model": facts.model.replace('_', " "),
        "width": facts.width,
        "height": facts.height,
        "orientation": orientation,
        "xscale": facts.xscale,
        "pointerSpeed": facts.pointer_speed,
        "hardwareSerial": facts.hardware_serial,
    })
}

/// `device_entry_dict`：adb 扫描到的一台设备（未与注册表合并前的默认值）。
pub fn device_entry_dict(entry: &DeviceEntry) -> Value {
    json!({
        "serial": entry.serial,
        "id": entry.serial,
        "state": entry.state,
        "model": entry.model.replace('_', " "),
        "transport": if entry.is_wireless() { "无线" } else { "USB" },
        "online": entry.online(),
        "remembered": false,
        "located": "",
        "locatedDetail": "",
        "lastSeen": "",
    })
}

/// `located_dict`：局域网查找结论。
pub fn located_dict(located: &Located) -> Value {
    json!({
        "status": located.status_str(),
        "address": located.address,
        "ip": located.ip,
        "mac": located.mac,
        "port": located.port,
        "detail": located.detail,
    })
}

/// `desktop_dict`：显示器 + 外轮廓边段 + 并集矩形。
pub fn desktop_dict(monitors: &[MonitorInfo], segments: &[EdgeSegment], bounds: &Rect) -> Value {
    let monitor_list: Vec<Value> = monitors.iter().map(monitor_dict).collect();
    let segment_list: Vec<Value> = segments.iter().map(segment_dict).collect();
    json!({
        "monitors": monitor_list,
        "segments": segment_list,
        "boundingBox": rect_dict(bounds),
    })
}

/// `config_dict`：配置树（含 `version`，不含 `issues`）。
pub fn config_dict(config: &Config) -> Result<Value, String> {
    config.to_dict().map_err(|err| err.to_string())
}

/// serde_json 值 → YAML 值（配置补丁重建用）。
pub fn json_to_yaml(value: &Value) -> serde_yaml_ng::Value {
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

/// `apply_config_patch`：按"当前值类型"逐字段强转，失败记一条中文问题。
///
/// 与 Python 的差异：Python 直接 setattr 到 dataclass 上；这里把补丁合并进
/// `to_dict()` 的 JSON 树再用 `Config::from_mapping` 重建（等价，且未知字段天然忽略）。
pub fn apply_config_patch(config: &mut Config, patch: &Value) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    let mut tree = match config.to_dict() {
        Ok(tree) => tree,
        Err(err) => {
            problems.push(format!("配置快照失败：{err}"));
            return problems;
        }
    };
    let Some(patch_map) = patch.as_object() else {
        return problems;
    };

    for (section, payload) in patch_map {
        if section == "layouts" {
            apply_layouts_patch(&mut tree, payload);
            continue;
        }
        let Some(payload_map) = payload.as_object() else {
            continue;
        };
        let Some(Value::Object(target)) = tree.get_mut(section) else {
            continue;
        };
        for (field, raw) in payload_map {
            let Some(current) = target.get(field).cloned() else {
                // Python 只遍历 dataclass 的字段：补丁里的未知字段直接忽略。
                continue;
            };
            match coerce_value(raw, &current) {
                Ok(value) => {
                    target.insert(field.clone(), value);
                }
                Err(err) => problems.push(format!("{section}.{field} 非法值：{err}")),
            }
        }
    }

    // from_mapping 会把越界值修回默认（Python Config 的 _repair）；为了像 Python
    // config_save 一样先报校验问题，这里对补丁后的树做一次只读校验。
    problems.extend(validate_tree(&tree));

    let yaml = json_to_yaml(&tree);
    if let Some(mapping) = yaml.as_mapping() {
        *config = Config::from_mapping(mapping);
    } else {
        problems.push("配置根节点必须是映射".to_string());
    }
    problems
}

/// 对补丁后的 JSON 树做与 Config::validate 同文案的校验（只读）。
fn validate_tree(tree: &Value) -> Vec<String> {
    let mut problems: Vec<String> = Vec::new();
    let device = tree.get("device");
    if let Some(port) = tree_i64(device.and_then(|value| value.get("server_port"))) {
        if !(1024..=65535).contains(&port) {
            problems.push("device.server_port 必须在 1024..65535".to_string());
        }
    }
    if let Some(placement) = tree.get("placement") {
        problems.extend(placement_tree_problems(placement, "placement"));
    }
    if let Some(devices) = tree
        .get("layouts")
        .and_then(|value| value.get("devices"))
        .and_then(Value::as_object)
    {
        for (serial, profiles) in devices {
            let Some(profiles) = profiles.as_object() else {
                continue;
            };
            for (orientation, profile) in profiles {
                problems.extend(placement_tree_problems(
                    profile,
                    &format!("layouts.devices.{serial}.{orientation}"),
                ));
            }
        }
    }
    let motion = tree.get("motion");
    if let Some(clamp) = tree_i64(motion.and_then(|value| value.get("hid_clamp"))) {
        if !(1..=127).contains(&clamp) {
            problems.push("motion.hid_clamp 必须在 1..127".to_string());
        }
    }
    if let Some(hz) = tree_i64(motion.and_then(|value| value.get("report_hz"))) {
        if !crate::constants::REPORT_HZ_CHOICES
            .iter()
            .any(|choice| i64::from(*choice) == hz)
        {
            problems.push("motion.report_hz 必须是 125/250/500/1000 之一".to_string());
        }
    }
    if let Some(gain) = tree_f64(motion.and_then(|value| value.get("gain"))) {
        if !(0.25..=3.0).contains(&gain) {
            problems.push("motion.gain 必须在 0.25..3.0".to_string());
        }
    }
    for (field, message) in [
        ("arm_px", "motion.arm_px 必须 ≥1"),
        ("arm_window_ms", "motion.arm_window_ms 必须 ≥1"),
        ("exit_overflow_px", "motion.exit_overflow_px 必须 ≥1"),
        ("edge_band_px", "motion.edge_band_px 必须 ≥1"),
    ] {
        if let Some(value) = tree_i64(motion.and_then(|section| section.get(field))) {
            if value < 1 {
                problems.push(message.to_string());
            }
        }
    }
    for (field, message) in [
        ("debounce_ms", "motion.debounce_ms 不能为负"),
        ("boundary_inset_px", "motion.boundary_inset_px 不能为负"),
    ] {
        if let Some(value) = tree_i64(motion.and_then(|section| section.get(field))) {
            if value < 0 {
                problems.push(message.to_string());
            }
        }
    }
    if let Some(layout) = tree
        .get("keyboard")
        .and_then(|value| value.get("layout"))
        .and_then(Value::as_str)
    {
        if layout.trim().is_empty() {
            problems.push("keyboard.layout 不能为空".to_string());
        }
    }
    for (field, message) in [
        ("toggle", "hotkeys.toggle 不能为空"),
        ("panic", "hotkeys.panic 不能为空"),
    ] {
        if let Some(value) = tree
            .get("hotkeys")
            .and_then(|section| section.get(field))
            .and_then(Value::as_str)
        {
            if value.trim().is_empty() {
                problems.push(message.to_string());
            }
        }
    }
    problems
}

fn placement_tree_problems(placement: &Value, prefix: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if let Some(edge) = placement.get("edge").and_then(Value::as_str) {
        if !crate::config::PLACEMENT_EDGES.contains(&edge) {
            problems.push(format!("{prefix}.edge 必须是 left/right/top/bottom"));
        }
    }
    if let Some(align) = placement.get("align").and_then(Value::as_str) {
        if !crate::config::ALIGNS.contains(&align) {
            problems.push(format!("{prefix}.align 必须是 proportional/center/one_to_one"));
        }
    }
    if let Some(corner) = placement.get("corner_priority").and_then(Value::as_str) {
        if !corner.is_empty() && !crate::config::PLACEMENT_EDGES.contains(&corner) {
            problems.push(format!("{prefix}.corner_priority 必须是合法边或空"));
        }
    }
    if let Some(offset) = tree_f64(placement.get("offset")) {
        if !(0.0..=1.0).contains(&offset) {
            problems.push(format!("{prefix}.offset 必须在 0.0..1.0"));
        }
    }
    let start = tree_i64(placement.get("span_start"));
    let end = tree_i64(placement.get("span_end"));
    if let (Some(start), Some(end)) = (start, end) {
        if end <= start {
            problems.push(format!("{prefix}.span_end 必须大于 span_start"));
        }
    }
    problems
}

fn tree_i64(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64)),
        _ => None,
    }
}

fn tree_f64(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64(),
        _ => None,
    }
}

/// `layouts.devices` 显式重建：只认 landscape/portrait，非法字段静默丢弃。
fn apply_layouts_patch(tree: &mut Value, payload: &Value) {
    let Some(devices) = payload.get("devices").and_then(Value::as_object) else {
        return;
    };
    let mut rebuilt: Map<String, Value> = Map::new();
    for (serial, profiles) in devices {
        let Some(profiles) = profiles.as_object() else {
            continue;
        };
        let mut buckets: Map<String, Value> = Map::new();
        for orientation in ["landscape", "portrait"] {
            let Some(raw) = profiles.get(orientation).and_then(Value::as_object) else {
                continue;
            };
            let mut placement: Map<String, Value> = Map::new();
            for (field, raw) in raw {
                let Some(kind) = placement_field_kind(field) else {
                    continue;
                };
                if let Ok(value) = coerce_kind(raw, kind) {
                    placement.insert(field.clone(), value);
                }
            }
            buckets.insert(orientation.to_string(), Value::Object(placement));
        }
        rebuilt.insert(serial.clone(), Value::Object(buckets));
    }
    if let Some(layouts) = tree.get_mut("layouts").and_then(Value::as_object_mut) {
        layouts.insert("devices".to_string(), Value::Object(rebuilt));
    }
}

/// 摆位字段的目标类型（与 `PlacementCfg` 的字段类型一致）。
fn placement_field_kind(field: &str) -> Option<Value> {
    match field {
        "offset" => Some(Value::from(0.0_f64)),
        "span_start" | "span_end" => Some(Value::from(0_i64)),
        "monitor" | "edge" | "align" | "corner_priority" => Some(Value::from("")),
        _ => None,
    }
}

fn coerce_kind(raw: &Value, kind: Value) -> Result<Value, String> {
    coerce_value(raw, &kind)
}

/// 把 `raw` 强转成 `expected` 的类型（Python `_coerce` 的等价实现）。
fn coerce_value(raw: &Value, expected: &Value) -> Result<Value, String> {
    match expected {
        Value::Bool(_) => Ok(Value::Bool(match raw {
            Value::Bool(flag) => *flag,
            Value::String(text) => matches!(
                text.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            ),
            Value::Number(number) => number.as_f64().map(|value| value != 0.0).unwrap_or(false),
            Value::Null => false,
            _ => true,
        })),
        Value::Number(number) if number.is_f64() => Ok(Value::from(number_of(raw)?)),
        Value::Number(_) => Ok(Value::from(integer_of(raw)?)),
        Value::String(_) => Ok(Value::String(text_of(raw))),
        Value::Array(_) => {
            let mut items: Vec<Value> = Vec::new();
            if let Some(list) = raw.as_array() {
                for item in list {
                    items.push(Value::String(text_of(item)));
                }
            }
            Ok(Value::Array(items))
        }
        Value::Object(_) => Ok(if raw.is_object() {
            raw.clone()
        } else {
            Value::Object(Map::new())
        }),
        other => Ok(other.clone()),
    }
}

fn integer_of(raw: &Value) -> Result<i64, String> {
    match raw {
        Value::Bool(flag) => Ok(i64::from(*flag)),
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|value| value as i64))
            .ok_or_else(|| "无效的整数".to_string()),
        Value::String(text) => text
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("无效的整数：{text}")),
        Value::Null => Ok(0),
        other => Err(format!("无效的整数：{other}")),
    }
}

fn number_of(raw: &Value) -> Result<f64, String> {
    match raw {
        Value::Bool(flag) => Ok(if *flag { 1.0 } else { 0.0 }),
        Value::Number(number) => number.as_f64().ok_or_else(|| "无效的数字".to_string()),
        Value::String(text) => text
            .trim()
            .parse::<f64>()
            .map_err(|_| format!("无效的数字：{text}")),
        Value::Null => Ok(0.0),
        other => Err(format!("无效的数字：{other}")),
    }
}

fn text_of(raw: &Value) -> String {
    match raw {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logic::geometry::Rect;

    fn facts() -> DeviceFacts {
        DeviceFacts {
            serial: "sim:5555".to_string(),
            model: "Simulated_Tablet".to_string(),
            width: 1707,
            height: 1067,
            orientation: 1,
            xscale: 2.0,
            pointer_speed: -3,
            hardware_serial: "HW1".to_string(),
        }
    }

    #[test]
    fn facts_dict_is_null_when_disconnected() {
        assert_eq!(facts_dict(None), Value::Null);
    }

    #[test]
    fn facts_dict_swaps_underscores_and_picks_landscape() {
        let value = facts_dict(Some(&facts()));
        assert_eq!(value["model"], "Simulated Tablet");
        assert_eq!(value["orientation"], "landscape");
        assert_eq!(value["pointerSpeed"], -3);
        assert_eq!(value["xscale"], 2.0);
        assert_eq!(value["hardwareSerial"], "HW1");
    }

    #[test]
    fn facts_dict_uses_orientation_bit_when_size_is_missing() {
        let mut facts = facts();
        facts.width = 0;
        facts.height = 0;
        facts.orientation = 1;
        assert_eq!(facts_dict(Some(&facts))["orientation"], "portrait");
        facts.orientation = 0;
        assert_eq!(facts_dict(Some(&facts))["orientation"], "landscape");
    }

    #[test]
    fn placement_dict_round_trips_through_from_dict() {
        let data = json!({
            "monitorHandle": 65537,
            "edge": "left",
            "spanStart": 0,
            "spanEnd": 1481,
            "offset": 0.25,
            "align": "center",
            "cornerPriority": "top",
        });
        let placement = placement_from_dict(&data);
        let value = placement_dict(Some(&placement));
        assert_eq!(value["monitorHandle"], 65537);
        assert_eq!(value["edge"], "left");
        assert_eq!(value["length"], 1481);
        assert_eq!(value["align"], "center");
        assert_eq!(value["cornerPriority"], "top");
        assert_eq!(value["offset"], 0.25);
    }

    #[test]
    fn placement_from_dict_falls_back_on_invalid_values() {
        let placement = placement_from_dict(&json!({"edge": "diagonal", "align": "stretch", "cornerPriority": "middle", "offset": 9}));
        assert_eq!(placement.segment.edge.as_str(), "right");
        assert_eq!(placement.align.as_str(), "proportional");
        assert!(placement.corner_priority.is_none());
        assert_eq!(placement.offset, 1.0);
    }

    #[test]
    fn placement_dict_is_null_when_missing() {
        assert_eq!(placement_dict(None), Value::Null);
    }

    #[test]
    fn desktop_dict_contains_three_sections() {
        let monitor = MonitorInfo::new(1, r"\\.\DISPLAY1", Rect::new(0, 0, 1920, 1080), 144, true);
        let bounds = Rect::new(0, 0, 1920, 1080);
        let value = desktop_dict(&[monitor], &[], &bounds);
        assert!(value["monitors"].is_array());
        assert_eq!(value["monitors"][0]["scale"], 1.5);
        assert_eq!(value["monitors"][0]["name"], "DISPLAY1");
        assert_eq!(value["boundingBox"]["width"], 1920);
        assert!(value["segments"].is_array());
    }

    #[test]
    fn device_entry_dict_marks_wireless_transport() {
        let mut entry = DeviceEntry::new("sim:5555", "device");
        entry.model = "Simulated_Tablet".to_string();
        let value = device_entry_dict(&entry);
        assert_eq!(value["transport"], "无线");
        assert_eq!(value["online"], true);
        assert_eq!(value["model"], "Simulated Tablet");
        assert_eq!(value["id"], "sim:5555");
        assert_eq!(value["remembered"], false);
    }

    #[test]
    fn located_dict_defaults_to_offline_status() {
        let located = Located::default();
        assert_eq!(located_dict(&located)["status"], "offline");
    }
}
