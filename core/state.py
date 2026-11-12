"""状态机定义与合法迁移表（设计文档 8.1，冻结契约）。

唯一写者是 Router（铁律 2）。本模块只提供枚举与合法性判定，不持有状态。
"""

from __future__ import annotations

from enum import StrEnum


class State(StrEnum):
    IDLE = "idle"
    ARMING = "arming"
    CAPTURED = "captured"
    RETURNING = "returning"
    FAULT = "fault"


LEGAL_TRANSITIONS: dict[State, frozenset[State]] = {
    State.IDLE: frozenset({State.ARMING, State.FAULT}),
    State.ARMING: frozenset({State.CAPTURED, State.IDLE, State.FAULT}),
    State.CAPTURED: frozenset({State.RETURNING, State.FAULT}),
    State.RETURNING: frozenset({State.IDLE, State.FAULT}),
    State.FAULT: frozenset({State.IDLE}),
}


def can_transition(src: State, dst: State) -> bool:
    """``src -> dst`` 是否合法。同状态不算迁移。"""
    return dst in LEGAL_TRANSITIONS.get(src, frozenset())
