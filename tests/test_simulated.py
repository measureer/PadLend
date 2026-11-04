"""``tablet.simulated`` 的单元测试：没有平板也要能验收输入链路。

这些替身只在 ``--simulate`` 与 ``tools/acceptance.py`` 里使用，正式路径不碰；
但一旦它们的行为和真会话不一致，验收结论就没有意义，所以同样要有断言。
"""

from __future__ import annotations

from tablet.adb import STATE_DEVICE
from tablet.simulated import SIM_SERIAL, SimulatedAdb, SimulatedSession


def test_session_reports_are_recorded() -> None:
    sim = SimulatedSession()
    assert sim.simulated is True
    assert sim.serial == SIM_SERIAL
    assert sim.started is False

    sim.start()
    assert sim.started is True
    assert sim.facts.serial == SIM_SERIAL
    assert sim.facts.width == sim.width

    sim.send_mouse(1, 4, -2)
    sim.send_mouse(0, 0, 0)
    assert sim.sent == 2
    assert sim.reports[-1].buttons == 0
    assert sim.clicks == 1
    assert (sim.reports[0].dx, sim.reports[0].dy) == (4, -2)


def test_clicks_counts_press_edges_only() -> None:
    sim = SimulatedSession()
    sim.start()
    for buttons in (1, 1, 0, 1, 0):
        sim.send_mouse(buttons, 0, 0)
    assert sim.clicks == 2


def test_session_keys_text_and_pointer_speed() -> None:
    sim = SimulatedSession()
    sim.start()
    sim.send_key(0, (4,))
    sim.inject_text("你好")
    sim.ensure_pointer_speed()
    sim.set_pointer_speed(-2)
    assert sim.keys == [(0, (4,))]
    assert sim.texts == ["你好"]
    assert sim.pointer_speed == -2
    assert sim.pointer_speed_calls == 1


def test_session_health_and_stop() -> None:
    sim = SimulatedSession()
    sim.start()
    healthy = sim.health()
    assert healthy.alive is True
    assert healthy.consecutive_failures == 0
    sim.stop()
    assert sim.started is False
    assert sim.health().alive is False


def test_simulated_adb_lists_one_online_device() -> None:
    adb = SimulatedAdb()
    entries = adb.devices()
    assert len(entries) == 1
    assert entries[0].serial == SIM_SERIAL
    assert entries[0].state == STATE_DEVICE
    assert entries[0].online is True
    assert adb.select_device(SIM_SERIAL).serial == SIM_SERIAL
    assert adb.wait_for_device().serial == SIM_SERIAL
    assert adb.pair("10.0.0.2:37000", "123456").startswith("Successfully paired")
    assert adb.connect("10.0.0.2:5555").startswith("connected to")
    adb.disconnect()
    adb.kill_server()
    assert adb.version() == "simulated"
    assert adb.with_serial(SIM_SERIAL) is adb
