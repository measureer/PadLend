"""facts 轮询的单元测试：一次变化必须落注册表、再套布局，且只推一次事件。"""

from __future__ import annotations

from pathlib import Path
from types import SimpleNamespace

from service.api import PadLendService


class _StopAfterOneTick:
    """模拟 _stop.wait：第一次返回 False（执行一轮），第二次返回 True（退出）。"""

    def __init__(self) -> None:
        self.calls = 0

    def wait(self, _timeout: float) -> bool:
        self.calls += 1
        return self.calls >= 2


def test_facts_loop_persists_identity_then_applies_layout(tmp_path: Path) -> None:
    svc = PadLendService(lambda *_: None, config_path=str(tmp_path / "config.yaml"))
    facts = SimpleNamespace(
        serial="USB1",
        model="Magic_Pad",
        width=3200,
        height=2136,
        orientation=0,
        xscale=1.0,
        pointer_speed=-3,
        hardware_serial="HW1",
    )
    events: list[tuple[str, object]] = []
    order: list[str] = []

    svc.emit = lambda event, data: events.append((event, data))
    svc.runtime = SimpleNamespace(router=SimpleNamespace(facts=facts))
    svc._stop = _StopAfterOneTick()

    real_remember = svc._remember_device

    def remember(serial: str, mac: str = "", facts: object = None) -> None:
        real_remember(serial, mac, facts)
        order.append("remember")

    svc._remember_device = remember
    svc._apply_layout_for_facts = lambda _facts: order.append("layout")

    svc._facts_loop()

    assert svc._stop.calls == 2
    assert svc._facts is facts
    assert [name for name, _ in events].count("facts") == 1
    assert events[0][0] == "facts"
    assert events[0][1]["serial"] == "USB1"
    assert events[0][1]["hardwareSerial"] == "HW1"
    assert order == ["remember", "layout"]

    record = svc.registry.find_by_hardware("HW1")
    assert record is not None
    assert record.model == "Magic_Pad"
    assert record.endpoints[0].serial == "USB1"
    assert svc.registry.path == tmp_path / "devices.json"
    assert svc.registry.path.exists()
    assert svc.cfg.device.serial == "USB1"
    assert svc.path.exists()
