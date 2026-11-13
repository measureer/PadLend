//! 状态机定义与合法迁移表（对应 `core/state.py`，设计文档 8.1，冻结契约）。
//!
//! 唯一写者是 Router（铁律 2）。本模块只提供枚举与合法性判定，不持有状态。
//!
//! 迁移表与 Python 版逐条一致，表顺序也保持 Python 字典的插入顺序
//! （idle / arming / captured / returning / fault），方便两侧导出结果直接对比。
//! `frozenset[State]` 用只读切片 `&'static [State]` 表达：不可变、可比较、
//! 不需要堆分配；`can_transition` 的语义（同状态不算迁移）完全照搬。

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// 状态枚举（Python `State(StrEnum)` 的 1:1 移植）。
///
/// 取值与 Python 完全一致：`idle` / `arming` / `captured` / `returning` / `fault`，
/// 因此 serde 序列化出来的就是这几个小写字符串（GUI 与日志都按它匹配）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// 空闲：没有接管平板。
    Idle,
    /// 预备：光标压线、正在确认是否要穿屏。
    Arming,
    /// 已接管：输入正在发往平板。
    Captured,
    /// 交还中：等待 debounce 结束回到 IDLE。
    Returning,
    /// 故障：设备/传输异常，只能回 IDLE 重新开始。
    Fault,
}

/// 合法迁移表（冻结契约，`core/state.py:LEGAL_TRANSITIONS` 的逐条翻译）。
///
/// 用 `(源状态, 合法目标集合)` 的有序表表达 Python 的
/// `dict[State, frozenset[State]]`；目标是 `&'static [State]`，只读且顺序稳定
/// （每一行的顺序与 Python 字面量相同）。新增/删除任何一条都必须同时改 Python
/// 版——两边是同一份契约。
pub const LEGAL_TRANSITIONS: &[(State, &[State])] = &[
    (State::Idle, &[State::Arming, State::Fault]),
    (
        State::Arming,
        &[State::Captured, State::Idle, State::Fault],
    ),
    (State::Captured, &[State::Returning, State::Fault]),
    (State::Returning, &[State::Idle, State::Fault]),
    (State::Fault, &[State::Idle]),
];

/// 全部状态，顺序与 Python 字典/枚举定义一致（GUI 画状态图时也用这个顺序）。
pub const ALL_STATES: [State; 5] = [
    State::Idle,
    State::Arming,
    State::Captured,
    State::Returning,
    State::Fault,
];

/// 状态名非法时的错误（Python `State("x")` 抛 `ValueError`）。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum StateError {
    /// 无法识别的状态字符串。
    #[error("'{value}' is not a valid State")]
    Unknown { value: String },
}

impl State {
    /// 枚举的小写字符串取值（等价于 Python StrEnum 的 `str(state)` / `.value`）。
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Arming => "arming",
            State::Captured => "captured",
            State::Returning => "returning",
            State::Fault => "fault",
        }
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for State {
    type Err = StateError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "idle" => Ok(State::Idle),
            "arming" => Ok(State::Arming),
            "captured" => Ok(State::Captured),
            "returning" => Ok(State::Returning),
            "fault" => Ok(State::Fault),
            other => Err(StateError::Unknown {
                value: other.to_string(),
            }),
        }
    }
}

impl TryFrom<&str> for State {
    type Error = StateError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// `src` 的合法目标集合；未列出的源状态返回空表
/// （对应 Python `.get(src, frozenset())`）。
pub fn legal_targets(src: State) -> &'static [State] {
    for (source, targets) in LEGAL_TRANSITIONS {
        if *source == src {
            return targets;
        }
    }
    &[]
}

/// `src -> dst` 是否合法。同状态不算迁移
/// （对应 `core/state.py:can_transition`）。
pub fn can_transition(src: State, dst: State) -> bool {
    legal_targets(src).contains(&dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐条对应 `core/state.py` 的 `LEGAL_TRANSITIONS` 字面量（冻结契约）。
    #[test]
    fn legal_transition_table_matches_frozen_contract() {
        let expected: &[(State, &[State])] = &[
            (State::Idle, &[State::Arming, State::Fault]),
            (
                State::Arming,
                &[State::Captured, State::Idle, State::Fault],
            ),
            (State::Captured, &[State::Returning, State::Fault]),
            (State::Returning, &[State::Idle, State::Fault]),
            (State::Fault, &[State::Idle]),
        ];
        assert_eq!(LEGAL_TRANSITIONS.len(), expected.len());
        for (actual, want) in LEGAL_TRANSITIONS.iter().zip(expected.iter()) {
            assert_eq!(actual.0, want.0);
            assert_eq!(actual.1, want.1);
        }
        // Python 字典的键顺序就是 ALL_STATES。
        let sources: Vec<State> = LEGAL_TRANSITIONS.iter().map(|(src, _)| *src).collect();
        assert_eq!(sources, ALL_STATES.to_vec());
    }

    /// 25 种组合里只有 10 条合法；任何同状态迁移都不合法。
    #[test]
    fn can_transition_covers_all_pairs() {
        let mut legal = 0;
        for src in ALL_STATES {
            for dst in ALL_STATES {
                let expected = matches!(
                    (src, dst),
                    (State::Idle, State::Arming)
                        | (State::Idle, State::Fault)
                        | (State::Arming, State::Captured)
                        | (State::Arming, State::Idle)
                        | (State::Arming, State::Fault)
                        | (State::Captured, State::Returning)
                        | (State::Captured, State::Fault)
                        | (State::Returning, State::Idle)
                        | (State::Returning, State::Fault)
                        | (State::Fault, State::Idle)
                );
                assert_eq!(can_transition(src, dst), expected, "{src} -> {dst}");
                if expected {
                    legal += 1;
                }
            }
        }
        assert_eq!(legal, 10);
        // 同状态一律不算迁移。
        for state in ALL_STATES {
            assert!(!can_transition(state, state));
        }
    }

    /// pytest `test_router.py` 里 IDLE -> RETURNING 被拒绝（`_change_state` 返回 False）。
    #[test]
    fn idle_to_returning_is_rejected() {
        assert!(!can_transition(State::Idle, State::Returning));
        assert_eq!(legal_targets(State::Idle), &[State::Arming, State::Fault]);
    }

    #[test]
    fn strings_round_trip() {
        for state in ALL_STATES {
            assert_eq!(state.to_string(), state.as_str());
            assert_eq!(state.as_str().parse::<State>(), Ok(state));
            assert_eq!(State::try_from(state.as_str()), Ok(state));
        }
        assert_eq!(State::Arming.as_str(), "arming");
        assert_eq!(
            "bogus".parse::<State>(),
            Err(StateError::Unknown {
                value: "bogus".to_string()
            })
        );
        assert_eq!(
            StateError::Unknown {
                value: "bogus".to_string()
            }
            .to_string(),
            "'bogus' is not a valid State"
        );
    }

    /// GUI / RPC 用的小写串必须与 Python StrEnum 的 `value` 一致。
    #[test]
    fn serde_uses_python_str_enum_values() {
        assert_eq!(
            serde_json::to_string(&State::Captured).unwrap(),
            "\"captured\""
        );
        let parsed: State = serde_json::from_str("\"returning\"").unwrap();
        assert_eq!(parsed, State::Returning);
        assert!(serde_json::from_str::<State>("\"bogus\"").is_err());
        assert_eq!(
            serde_yaml_ng::to_string(&State::Fault).unwrap().trim(),
            "fault"
        );
    }
}
