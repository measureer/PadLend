"""PadLend 核心层：状态机（唯一写者）、事件、指标、错误码。

依赖方向（7.1）：ui → core → win/tablet → constants。
core 运行时不 import win/tablet：所需能力通过构造注入（见 Router）。
"""
