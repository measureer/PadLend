"""业务门面：把 ``core/``、``tablet/``、``win/`` 的能力暴露成稳定 RPC 方法。

设计原则：
* UI 层不再持有 Router / Adb / DesktopMap 对象，也不再自己拼装业务规则；
* 所有慢操作（adb、子进程、诊断、mDNS/ARP、连接等待）由 server 的线程池调度，
  本模块里的函数因此可以同步写，但永远不会跑在界面线程上；
* 状态变化通过事件推送（state/metrics/facts/devices/placement/log），
  替换掉旧版 1s GUI 定时器轮询与全量控件重建。
"""

from __future__ import annotations

import contextlib
import copy
import logging
import os
import subprocess
import threading
import time
from collections import deque
from collections.abc import Callable
from datetime import datetime
from pathlib import Path
from typing import Any

import constants as C
from config import Config, PlacementCfg, load_config
from core.diagnostics import (
    DIAGNOSTIC_STEPS,
    CommandResult,
    DiagnosticsReport,
    build_report,
    export_report,
    static_summary,
)
from core.geometry import Align, Edge, EdgeSegment, Placement
from main import Runtime, _mark_clean_exit, _mark_running, _set_dpi_awareness, _stale_cleanup
from service.serializers import (
    apply_config_patch,
    config_dict,
    desktop_dict,
    device_entry_dict,
    facts_dict,
    located_dict,
    placement_dict,
    placement_from_dict,
)
from tablet.adb import normalize_address
from tablet.locator import (
    STATUS_MOVED,
    STATUS_ONLINE,
    Located,
    arp_table,
    discovered_services,
    locate,
    normalize_mac,
)

log = logging.getLogger("service.api")

Emit = Callable[[str, Any], None]


def _attr(obj: Any, name: str, default: Any = None) -> Any:
    if isinstance(obj, dict):
        return obj.get(name, default)
    return getattr(obj, name, default)


class PadLendService:
    """一个进程一份；持有配置、引擎和最近一次快照。"""

    def __init__(
        self,
        emit: Emit,
        *,
        simulate: bool = False,
        no_hooks: bool = False,
        config_path: str | None = None,
    ) -> None:
        self.emit = emit
        self.simulate = bool(simulate)
        self.no_hooks = bool(no_hooks)
        self.path = Path(config_path) if config_path else C.config_path()
        self.cfg: Config = Config()
        self.runtime: Runtime | None = None
        self.hotkeys: Any = None
        self.first_run = False
        self.started = False
        self._stop = threading.Event()
        self._lock = threading.RLock()
        self._facts: Any = None
        self._facts_key: tuple[Any, ...] | None = None
        self._placement: Placement | None = None
        self._layout_key: tuple[str, str] | None = None
        self._located: dict[str, dict[str, Any]] = {}
        self._known: list[str] = []
        self._devices: list[dict[str, Any]] = []
        self._logs: deque[dict[str, str]] = deque(maxlen=1000)
        self._report: DiagnosticsReport | None = None
        self._metrics: dict[str, Any] = {}
        self._state: str = "idle"
        self._shutdown_done = False
        self._shutdown_lock = threading.Lock()
        self._ready = threading.Event()
        self._start_error: str = ""

    # ------------------------------------------------------------------ 生命周期
    def wait_ready(self, timeout: float = 30.0) -> None:
        """RPC 处理器等待引擎组装完成；失败时直接把原因回报给界面。"""
        if not self._ready.wait(timeout):
            raise RuntimeError("业务服务启动超时")
        if self._start_error:
            raise RuntimeError(self._start_error)

    def start(self) -> None:
        try:
            self._start()
        except Exception as exc:  # noqa: BLE001 - 启动失败也要给前端一个明确答复
            log.exception("service startup failed")
            self._start_error = str(exc)
            self.emit("fatal", {"message": str(exc)})
        finally:
            self._ready.set()

    def _start(self) -> None:
        _set_dpi_awareness()
        self.first_run = not self.path.exists()
        self.cfg = load_config(self.path)
        self.cfg.validate()
        if self.first_run:
            try:
                self.cfg.save(self.path)
            except OSError as exc:
                log.warning("无法写入默认配置：%r", exc)
        if not self.simulate:
            # 上次异常退出的残留清理；这是进程启动阶段的慢操作，
            # 但它在服务进程而非 WebView 里，界面从第一帧起就是可交互的。
            _stale_cleanup(self.cfg, first_run=self.first_run)
        _mark_running()

        self.runtime = Runtime(self.cfg, simulate=self.simulate)
        router = self.runtime.router
        router.on_state_changed = self._on_state_changed
        router.on_metrics = self._on_metrics
        router.on_orientation_changed = self._on_orientation
        router.on_clipboard_received = self._on_clipboard

        if self.no_hooks:
            router.start()
        else:
            self.runtime.start()
            try:
                self.runtime.keyboard_hook.install()
                log.info("键盘钩子已安装")
            except Exception as exc:  # noqa: BLE001 - 钩子失败不阻断鼠标链路
                log.warning("键盘钩子不可用：%r", exc)
            self._start_hotkeys()

        initial = self._placement_from_config()
        if initial is not None:
            self._placement = initial
            try:
                router.set_placement(initial)
            except Exception as exc:  # noqa: BLE001
                log.warning("router.set_placement(initial) failed: %r", exc)

        self._state = str(getattr(router.state, "value", router.state))
        threading.Thread(target=self._facts_loop, name="service-facts", daemon=True).start()
        self.emit_log("APP", f"{C.APP_NAME} {C.VERSION} 业务服务已就绪")
        self.started = True

    def _start_hotkeys(self) -> None:
        if os.name != "nt" or self.runtime is None:
            return
        from win.hotkeys import HotkeyManager

        def _toggle() -> None:
            try:
                self.runtime.router.toggle_manual()
            except Exception as exc:  # noqa: BLE001 - 热键线程不能抛
                log.warning("toggle_manual failed: %r", exc)

        def _panic() -> None:
            self.engine_panic({})

        try:
            self.hotkeys = HotkeyManager(_toggle, _panic)
            self.hotkeys.start()
        except Exception as exc:  # noqa: BLE001
            log.warning("全局热键不可用：%r", exc)
            self.hotkeys = None

    def shutdown(self) -> None:
        """同步 teardown：返回时遮罩/ClipCursor/UHID/adb 都已释放。

        1.1.2 把清理丢进 daemon 线程后立刻返回，主线程可能先退出，遮罩释放、
        UHID 移除、exit.clean 写入都可能被截断；退出路径必须同步。
        重复调用安全：第二次直接返回，不会重复 kill 子进程。
        """
        with self._shutdown_lock:
            if self._shutdown_done:
                return
            self._shutdown_done = True
        self._stop.set()
        if self.hotkeys is not None:
            with contextlib.suppress(Exception):
                self.hotkeys.stop()
            self.hotkeys = None
        if self.runtime is not None:
            with contextlib.suppress(Exception):
                self.runtime.stop()
        _mark_clean_exit()

    # ------------------------------------------------------------------ 事件
    def emit_log(self, event: str, message: str) -> None:
        stamp = datetime.now().strftime("%H:%M:%S.") + f"{datetime.now().microsecond // 1000:03d}"
        entry = {"timestamp": stamp, "event": event, "message": str(message)}
        self._logs.append(entry)
        log.info("%s %s", event, message, extra={"ui_logged": True})
        self.emit("log", entry)

    def _on_state_changed(self, old: Any, new: Any, reason: Any) -> None:
        old_text = str(getattr(old, "value", old) or "")
        new_text = str(getattr(new, "value", new) or "")
        self._state = new_text
        self.emit("state", {"old": old_text, "new": new_text, "reason": str(reason or "")})
        self.emit_log("STATE", f"{old_text or '—'} → {new_text or '—'}（{reason or ''}）")

    def _on_metrics(self, snapshot: dict[str, Any]) -> None:
        if not isinstance(snapshot, dict):
            return
        # 与 core.metrics.snapshot() 的键名保持一致，前端不再做别名猜测。
        self._metrics = dict(snapshot)
        self.emit("metrics", dict(snapshot))

    def _on_orientation(self, facts: Any) -> None:
        if facts is None:
            return
        bucket = self._orientation_bucket(facts)
        self.emit("orientation", facts_dict(facts))
        self.emit_log("ORIENT", f"平板旋转为{'横屏' if bucket == 'landscape' else '竖屏'}")

    def _on_clipboard(self, text: Any) -> None:
        if text is not None:
            self.emit("clipboard", {"text": str(text)})

    def _facts_loop(self) -> None:
        """替代旧版 GUI 线程上的 1000ms QTimer：事实变化才推一次事件。"""
        while not self._stop.wait(0.5):
            runtime = self.runtime
            if runtime is None:
                continue
            try:
                facts = runtime.router.facts
            except Exception:  # noqa: BLE001
                continue
            key = None
            if facts is not None:
                key = (
                    _attr(facts, "serial", ""),
                    _attr(facts, "width", 0),
                    _attr(facts, "height", 0),
                    _attr(facts, "orientation", 0),
                    _attr(facts, "xscale", 0),
                )
            if key == self._facts_key:
                continue
            self._facts_key = key
            self._facts = facts
            dto = facts_dict(facts)
            self.emit("facts", dto)
            if dto is not None:
                self._apply_layout_for_facts(facts)
                self._remember_device(dto["serial"])
                self.emit_log("DEVICE", f"设备事实：{dto['serial']} {dto['width']}×{dto['height']}")
            elif self._layout_key is not None:
                self._layout_key = None

    # ------------------------------------------------------------------ 快照
    def snapshot(self) -> dict[str, Any]:
        runtime = self.runtime
        desktop = getattr(runtime, "desktop", None) if runtime is not None else None
        return {
            "version": C.VERSION,
            "config": config_dict(self.cfg),
            "configIssues": list(self.cfg.issues),
            "configPath": str(self.path),
            "firstRun": self.first_run,
            "simulated": self.simulate,
            "theme": str(getattr(self.cfg.ui, "theme", "dark")),
            "state": self._state,
            "connected": bool(self._facts is not None),
            "facts": facts_dict(self._facts),
            "metrics": dict(self._metrics),
            "desktop": desktop_dict(desktop)
            if desktop is not None
            else {"monitors": [], "segments": [], "boundingBox": {}},
            "placement": placement_dict(self._placement),
            "devices": list(self._devices),
            "logs": list(self._logs)[-200:],
        }

    # ------------------------------------------------------------------ RPC 分派
    def call(self, method: str, params: dict[str, Any]) -> Any:
        handlers: dict[str, Callable[[dict[str, Any]], Any]] = {
            "app.bootstrap": self.app_bootstrap,
            "app.shutdown": self.app_shutdown,
            "config.get": self.config_get,
            "config.save": self.config_save,
            "devices.scan": self.devices_scan,
            "devices.locate": self.devices_locate,
            "devices.connect": self.devices_connect,
            "devices.disconnect": self.devices_disconnect,
            "devices.add": self.devices_add,
            "devices.remove": self.devices_remove,
            "layout.desktop": self.layout_desktop,
            "layout.initial": self.layout_initial,
            "layout.save": self.layout_save,
            "engine.toggle": self.engine_toggle,
            "engine.panic": self.engine_panic,
            "engine.reconnect": self.engine_reconnect,
            "engine.inject_clipboard": self.engine_inject_clipboard,
            "engine.state": self.engine_state,
            "engine.metrics": self.engine_metrics,
            "diagnostics.run": self.diagnostics_run,
            "diagnostics.export": self.diagnostics_export,
            "diagnostics.static": self.diagnostics_static,
            "system.paths": self.system_paths,
            "wizard.check": self.wizard_check,
            "wizard.push_jar": self.wizard_push_jar,
            "wizard.finish": self.wizard_finish,
            "logs.recent": self.logs_recent,
        }
        handler = handlers.get(method)
        if handler is None:
            raise KeyError(f"未知方法：{method}")
        return handler(params or {})

    # ------------------------------------------------------------------ app / config
    def app_bootstrap(self, _params: dict[str, Any]) -> dict[str, Any]:
        return self.snapshot()

    def app_shutdown(self, _params: dict[str, Any]) -> dict[str, Any]:
        self.shutdown()
        return {"ok": True}

    def config_get(self, _params: dict[str, Any]) -> dict[str, Any]:
        return {
            "config": config_dict(self.cfg),
            "issues": list(self.cfg.issues),
            "path": str(self.path),
        }

    def config_save(self, params: dict[str, Any]) -> dict[str, Any]:
        patch = params.get("patch") or {}
        if not isinstance(patch, dict):
            raise ValueError("配置补丁必须是对象")
        with self._lock:
            # 补丁是「先改后校验」，失败必须整体回滚，否则一次非法输入会污染
            # 运行中的 Config（Router 持有同一个对象引用）。
            backup = copy.deepcopy(self.cfg)
            problems = apply_config_patch(self.cfg, patch)
            problems += self.cfg.validate()
            if problems:
                self.cfg.__dict__.update(backup.__dict__)
                raise ValueError("；".join(problems))
            self.cfg.save(self.path)
            self.emit("config", {"config": config_dict(self.cfg), "issues": []})
        self.emit_log("CONFIG", f"配置已保存：{self.path}")
        return {"config": config_dict(self.cfg), "issues": [], "path": str(self.path)}

    # ------------------------------------------------------------------ 设备
    def _known_devices(self) -> list[str]:
        known: list[str] = []
        current = str(getattr(self.cfg.device, "serial", "") or "")
        for serial in [current, *(getattr(self.cfg.device, "known", []) or [])]:
            text = str(serial or "").strip()
            if text and text not in known:
                known.append(text)
        return known[:8]

    def _publish_devices(self, entries: list[Any], error: str = "") -> list[dict[str, Any]]:
        devices: list[dict[str, Any]] = []
        seen: set[str] = set()
        for entry in entries:
            dto = device_entry_dict(entry)
            serial = dto["serial"]
            if not serial or serial in seen:
                continue
            seen.add(serial)
            dto["located"] = self._located.get(serial, {}).get("status", "")
            dto["locatedDetail"] = self._located.get(serial, {}).get("detail", "")
            devices.append(dto)
        for serial in self._known_devices():
            if serial in seen:
                continue
            located = self._located.get(serial, {})
            devices.append(
                {
                    "serial": serial,
                    "state": "offline",
                    "model": "上次连接的设备",
                    "transport": "无线" if ":" in serial else "USB",
                    "online": False,
                    "remembered": True,
                    "located": located.get("status", "offline"),
                    "locatedDetail": located.get("detail", "上次连接，当前未接入"),
                }
            )
        self._devices = devices
        self.emit("devices", {"devices": devices, "error": error})
        return devices

    def devices_scan(self, _params: dict[str, Any]) -> dict[str, Any]:
        adb = getattr(self.runtime, "adb", None) if self.runtime is not None else None
        entries: list[Any] = []
        error = ""
        try:
            if adb is not None and callable(getattr(adb, "devices", None)):
                entries = list(adb.devices())
            else:
                error = "adb 接口不可用"
        except Exception as exc:  # noqa: BLE001 - 扫描失败只影响列表
            error = str(exc)
            log.info("devices scan failed: %r", exc)
        devices = self._publish_devices(entries, error)
        if error:
            hint = "未找到 adb（打包版应内置）" if "adb" in error.lower() else error
        else:
            hint = f"{sum(1 for d in devices if d['online'])} 台设备"
        self.emit_log("DEVICE", f"扫描设备：{hint}")
        return {"devices": devices, "hint": hint, "error": error}

    def devices_locate(self, _params: dict[str, Any]) -> dict[str, Any]:
        adb = getattr(self.runtime, "adb", None)
        if adb is None or str(getattr(adb, "path", "")) == "simulated-adb":
            return {"located": dict(self._located)}
        targets = [
            (s, str((self.cfg.device.macs or {}).get(s, "") or ""))
            for s in self._known_devices()
            if ":" in s
        ]
        if not targets:
            return {"located": dict(self._located)}
        try:
            services = getattr(adb, "mdns_services", None)
            discovered = discovered_services(services if callable(services) else None)
        except Exception as exc:  # noqa: BLE001
            log.info("mDNS discovery failed: %r", exc)
            discovered = []
        for serial, mac in targets:
            try:
                found = locate(serial, mac, mdns=lambda: discovered, arp=arp_table)
            except Exception as exc:  # noqa: BLE001
                found = Located(detail=f"查找失败：{exc}")
            if found.status == STATUS_MOVED and found.address:
                self._adopt_address(serial, found)
                self._located[found.address] = located_dict(found)
                self.emit_log("DEVICE", f"平板换了地址：{serial} → {found.address}")
            else:
                self._located[serial] = located_dict(found)
            if found.status == STATUS_ONLINE and not self.cfg.device.macs.get(serial):
                self._capture_mac(serial)
        self._publish_devices([], "")
        online = sum(1 for item in self._located.values() if item.get("status") == STATUS_ONLINE)
        self.emit_log("DEVICE", f"局域网查找：{online} 台在线")
        return {"located": dict(self._located)}

    def devices_connect(self, params: dict[str, Any]) -> dict[str, Any]:
        serial = str(params.get("serial") or "").strip()
        if not serial:
            raise ValueError("请选择一个已授权的设备")
        router = getattr(self.runtime, "router", None)
        if router is None:
            raise RuntimeError("引擎未启动")
        self.emit_log("DEVICE", f"连接请求：{serial}")
        if ":" in serial and self.runtime is not None:
            connect = getattr(self.runtime.adb, "connect", None)
            if callable(connect):
                try:
                    connect(serial)
                except Exception as exc:  # noqa: BLE001 - 由下面的超时兜底
                    log.info("adb connect %s failed: %r", serial, exc)
        router.connect_device(serial)
        deadline = time.monotonic() + 45.0
        while time.monotonic() < deadline and not self._stop.is_set():
            facts = router.facts
            if facts is not None and _attr(facts, "serial", ""):
                self.emit_log("DEVICE", f"已连接 {serial}")
                return {"ok": True, "message": ""}
            time.sleep(0.1)
        message = "连接超时：请确认 USB 调试已授权，且 adb 能看到设备"
        self.emit_log("DEVICE", f"操作失败：{message}")
        return {"ok": False, "message": message}

    def devices_disconnect(self, _params: dict[str, Any]) -> dict[str, Any]:
        router = getattr(self.runtime, "router", None)
        if router is None:
            raise RuntimeError("引擎未启动")
        router.disconnect_device()
        self.emit_log("DEVICE", "已断开")
        return {"ok": True}

    def devices_add(self, params: dict[str, Any]) -> dict[str, Any]:
        address = normalize_address(str(params.get("address") or ""))
        code = str(params.get("code") or "").strip()
        if not address:
            raise ValueError("请填写平板上的 IP 地址与端口")
        adb = getattr(self.runtime, "adb", None)
        if adb is None:
            raise RuntimeError("adb 接口不可用")
        if code:
            adb.pair(address, code)
        if callable(getattr(adb, "connect", None)):
            adb.connect(address)
        return self.devices_connect({"serial": address})

    def devices_remove(self, params: dict[str, Any]) -> dict[str, Any]:
        serial = str(params.get("serial") or "").strip()
        if not serial:
            return {"ok": False}
        with self._lock:
            self.cfg.device.known = [s for s in (self.cfg.device.known or []) if str(s) != serial]
            self.cfg.device.macs.pop(serial, None)
            if str(getattr(self.cfg.device, "serial", "")) == serial:
                known = self.cfg.device.known
                self.cfg.device.serial = known[0] if known else ""
            self._located.pop(serial, None)
            self.cfg.save(self.path)
        self.emit("config", {"config": config_dict(self.cfg), "issues": []})
        self.emit_log("DEVICE", f"已从列表移除 {serial}")
        return {"ok": True, "config": config_dict(self.cfg)}

    def _remember_device(self, serial: str, mac: str = "") -> None:
        text = str(serial or "").strip()
        if not text:
            return
        with self._lock:
            if text in self._known_devices() and not mac:
                return
            learned = normalize_mac(mac)
            if learned:
                self.cfg.device.macs[text] = learned
            known = [text, *(s for s in self._known_devices() if s != text)]
            self.cfg.device.known = known[:8]
            self.cfg.device.serial = text
            keep = set(self.cfg.device.known)
            self.cfg.device.macs = {
                k: v for k, v in (self.cfg.device.macs or {}).items() if k in keep and v
            }
            self.cfg.save(self.path)
        self.emit("config", {"config": config_dict(self.cfg), "issues": []})
        if ":" in text and not self.cfg.device.macs.get(text):
            self._capture_mac(text)

    def _capture_mac(self, serial: str) -> None:
        if ":" not in serial or self.simulate:
            return

        def _run() -> None:
            adb = getattr(self.runtime, "adb", None)
            if adb is None:
                return
            try:
                mac = adb.wifi_mac(serial)
            except Exception as exc:  # noqa: BLE001
                log.info("wifi_mac failed: %r", exc)
                return
            if mac:
                self._remember_device(serial, mac=mac)

        threading.Thread(target=_run, name="service-mac", daemon=True).start()

    def _adopt_address(self, previous: str, found: Located) -> None:
        target = str(getattr(found, "address", "") or "").strip()
        if not target or target == previous:
            return
        with self._lock:
            old_mac = normalize_mac((self.cfg.device.macs or {}).get(previous, ""))
            learned = normalize_mac(getattr(found, "mac", "")) or old_mac
            self.cfg.device.macs.pop(previous, None)
            self.cfg.device.known = [
                target,
                *(s for s in (self.cfg.device.known or []) if s not in (previous, target)),
            ][:8]
            self.cfg.device.serial = target
            self._remember_device(target, mac=learned)

    # ------------------------------------------------------------------ 布局
    def _monitor_device(self, handle: int) -> str:
        desktop = getattr(self.runtime, "desktop", None)
        for monitor in getattr(desktop, "monitors", ()) or ():
            if int(_attr(monitor, "handle", 0) or 0) == handle:
                return str(_attr(monitor, "device", "") or "")
        return ""

    def _segments(self) -> list[EdgeSegment]:
        desktop = getattr(self.runtime, "desktop", None)
        try:
            return list(desktop.exterior_segments())
        except Exception:  # noqa: BLE001
            return []

    def _placement_from_config(
        self, source: Any = None, *, strict_monitor: bool = False
    ) -> Placement | None:
        cfg = self.cfg.placement if source is None else source
        segments = self._segments()
        if not segments:
            return None
        edge_text = str(_attr(cfg, "edge", "") or "")
        monitor = str(_attr(cfg, "monitor", "") or "").casefold()

        def same_edge(segment: EdgeSegment) -> bool:
            return str(_attr(segment.edge, "value", segment.edge)) == edge_text

        candidates = [s for s in segments if same_edge(s)]
        if monitor:
            exact = [
                s
                for s in candidates
                if self._monitor_device(s.monitor_handle).casefold() == monitor
            ]
            if exact:
                candidates = exact
            elif strict_monitor:
                return None
        if not candidates:
            if strict_monitor and edge_text:
                return None
            candidates = segments
        segment = max(candidates, key=lambda item: item.length)
        try:
            align = Align(str(_attr(cfg, "align", "") or "proportional"))
        except ValueError:
            align = Align.PROPORTIONAL
        corner_text = str(_attr(cfg, "corner_priority", "") or "")
        try:
            corner = Edge(corner_text) if corner_text else None
        except ValueError:
            corner = None
        try:
            offset = max(0.0, min(1.0, float(_attr(cfg, "offset", 0.5))))
        except (TypeError, ValueError):
            offset = 0.5
        return Placement(segment, offset, align, corner)

    def _apply_placement_to_config(self, placement: Placement, target: Any = None) -> None:
        cfg = target if target is not None else self.cfg.placement
        segment = placement.segment
        cfg.edge = str(_attr(segment.edge, "value", cfg.edge))
        cfg.span_start = int(segment.span_start)
        cfg.span_end = int(segment.span_end)
        cfg.monitor = self._monitor_device(segment.monitor_handle) or cfg.monitor
        cfg.offset = float(placement.offset)
        cfg.align = str(_attr(placement.align, "value", cfg.align))
        cfg.corner_priority = (
            str(_attr(placement.corner_priority, "value", "")) if placement.corner_priority else ""
        )

    def _layout_identity(self, facts: Any = None) -> str:
        source = self._facts if facts is None else facts
        if source is None:
            return ""
        return str(
            _attr(source, "hardware_serial", "") or _attr(source, "serial", "") or ""
        ).strip()

    @staticmethod
    def _orientation_bucket(facts: Any) -> str:
        if facts is None:
            return ""
        width = int(_attr(facts, "width", 0) or 0)
        height = int(_attr(facts, "height", 0) or 0)
        if width and height:
            return "landscape" if width >= height else "portrait"
        return "portrait" if int(_attr(facts, "orientation", 0) or 0) & 1 else "landscape"

    def _apply_layout_for_facts(self, facts: Any) -> None:
        """设备/方向变化自动切换已保存的布局（原主窗逻辑下沉到服务层）。"""
        identity = self._layout_identity(facts)
        bucket = self._orientation_bucket(facts)
        if not identity or not bucket:
            return
        key = (identity.casefold(), bucket)
        if key == self._layout_key:
            return
        previous = self._layout_key
        profile = self.cfg.layouts.get(identity, bucket)
        if profile is not None:
            placement = self._placement_from_config(profile, strict_monitor=True)
            if placement is None:
                log.warning("layout profile has no matching segment key=%s", key)
                return
            self._layout_key = key
            self._placement = placement
            self.runtime.router.set_placement(placement)
            self.emit(
                "placement",
                {"placement": placement_dict(placement), "dirty": False, "reason": "profile"},
            )
            self.emit_log(
                "LAYOUT", f"已切换到该设备的{'横屏' if bucket == 'landscape' else '竖屏'}布局"
            )
            return
        self._layout_key = key
        if previous is not None and previous[0] == key[0]:
            return
        placement = self._placement_from_config()
        if placement is None:
            return
        self._placement = placement
        self.runtime.router.set_placement(placement)
        self.emit(
            "placement",
            {"placement": placement_dict(placement), "dirty": False, "reason": "global"},
        )

    def layout_desktop(self, _params: dict[str, Any]) -> dict[str, Any]:
        desktop = getattr(self.runtime, "desktop", None)
        if desktop is not None and callable(getattr(desktop, "refresh", None)):
            with __import__("contextlib").suppress(Exception):
                desktop.refresh()
        return {
            **desktop_dict(desktop),
            "placement": placement_dict(self._placement),
            "tablet": facts_dict(self._facts),
        }

    def layout_initial(self, _params: dict[str, Any]) -> dict[str, Any]:
        return {"placement": placement_dict(self._placement)}

    def layout_save(self, params: dict[str, Any]) -> dict[str, Any]:
        data = params.get("placement")
        if not isinstance(data, dict):
            raise ValueError("没有可保存的布局")
        placement = placement_from_dict(data)
        if self._monitor_device(placement.segment.monitor_handle) == "":
            raise ValueError("摆位对应的显示器已不存在，请重新拖动")
        identity = str(params.get("identity") or "") or self._layout_identity()
        bucket = str(params.get("orientation") or "") or self._orientation_bucket(self._facts)
        with self._lock:
            if identity and bucket in ("landscape", "portrait"):
                profile = PlacementCfg()
                self._apply_placement_to_config(placement, profile)
                self.cfg.layouts.set(identity, bucket, profile)
            self._apply_placement_to_config(placement)
            # 布局页只对自己负责的字段做校验，不因别处遗留的非法值被挡住
            # （与旧版 _save_layout → _save_config 的行为一致）。
            problems = [p for p in self.cfg.validate() if p.startswith("placement.")]
            if problems:
                raise ValueError("；".join(problems))
            self.cfg.save(self.path)
        if self.runtime is not None:
            self.runtime.router.set_placement(placement)
        self._placement = placement
        self._layout_key = (identity.casefold(), bucket) if identity and bucket else None
        self.emit(
            "placement", {"placement": placement_dict(placement), "dirty": False, "reason": "saved"}
        )
        self.emit("config", {"config": config_dict(self.cfg), "issues": []})
        self.emit_log(
            "PLACE",
            f"布局已保存（{'横屏' if bucket == 'landscape' else '竖屏'}）："
            f"吸附 {placement.segment.edge.value} offset={placement.offset:.2f}",
        )
        return {"config": config_dict(self.cfg), "placement": placement_dict(placement)}

    # ------------------------------------------------------------------ 引擎
    def engine_toggle(self, _params: dict[str, Any]) -> dict[str, Any]:
        if self.runtime is not None:
            self.runtime.router.toggle_manual()
        self.emit_log("MANUAL", "手动接管 / 交还")
        return {"ok": True}

    def engine_panic(self, _params: dict[str, Any]) -> dict[str, Any]:
        if self.runtime is not None:
            action = (
                getattr(self.runtime.router, "emergency_stop", None) or self.runtime.router.stop
            )
            action()
            with __import__("contextlib").suppress(Exception):
                self.runtime.mask.release()
        self.emit_log("PANIC", "急停：已释放遮罩与输入")
        return {"ok": True}

    def engine_reconnect(self, _params: dict[str, Any]) -> dict[str, Any]:
        if self.runtime is not None:
            self.runtime.router.resync_pointer()
        self.emit_log("RECONNECT", "重新同步指针")
        return {"ok": True}

    def engine_inject_clipboard(self, params: dict[str, Any]) -> dict[str, Any]:
        text = str(params.get("text") or "")
        ok = False
        if self.runtime is not None and text:
            ok = bool(self.runtime.router.inject_clipboard(text))
        return {"ok": ok}

    def engine_state(self, _params: dict[str, Any]) -> dict[str, Any]:
        router = getattr(self.runtime, "router", None)
        return {
            "state": self._state,
            "connected": bool(self._facts is not None),
            "facts": facts_dict(self._facts),
            "metrics": dict(self._metrics),
            "manual": bool(getattr(router, "manual", False)),
        }

    def engine_metrics(self, _params: dict[str, Any]) -> dict[str, Any]:
        snap = getattr(self.runtime.router, "metrics_snapshot", None) if self.runtime else None
        return dict(snap() if callable(snap) else self._metrics)

    # ------------------------------------------------------------------ 诊断
    def _run_adb(self, args: tuple[str, ...], timeout: float = 12.0) -> tuple[int, str, str]:
        adb = getattr(self.runtime, "adb", None)
        path = str(_attr(adb, "path", "") or "") if adb is not None else ""
        if self.simulate or path == "simulated-adb":
            return 0, self._simulated_adb_output(args), ""
        exe = path or C.ADB_EXE
        try:
            done = subprocess.run(
                [exe, *args],
                capture_output=True,
                timeout=timeout,
                text=True,
                encoding="utf-8",
                errors="replace",
                check=False,
            )
            return done.returncode, done.stdout, done.stderr
        except (OSError, subprocess.TimeoutExpired) as exc:
            return -1, "", str(exc)

    @staticmethod
    def _simulated_adb_output(args: tuple[str, ...]) -> str:
        head = args[0] if args else ""
        if head == "devices":
            return (
                "List of devices attached\n"
                "SIMULATEDPAD\tdevice usb:1-1 product:sim model:PadLend_Demo\n"
            )
        if args[:3] == ("shell", "dumpsys", "input"):
            return "Cursor Input Mapper:\n  XScale: 1.000\n"
        if "pointer_speed" in args:
            return "-3\n"
        if args[:2] == ("shell", "id"):
            return "uid=2000(shell) gid=2000(shell) groups=1003,uhid,1000\n"
        if "uhid" in " ".join(args):
            return "OPEN_OK\n"
        if "adb_enabled" in " ".join(args):
            return "1\n"
        return ""

    def diagnostics_run(self, _params: dict[str, Any]) -> dict[str, Any]:
        results: list[CommandResult] = []
        for label, args in DIAGNOSTIC_STEPS:
            code, stdout, stderr = self._run_adb(args)
            results.append(CommandResult(label, code, stdout, stderr))
            self.emit("diagnostics.progress", {"label": label})
        report = build_report(results)
        self._report = report
        device = report.primary_device()
        self.emit_log("DIAG", "诊断采集完成")
        return {
            "text": report.to_text(),
            "summary": report.summary(),
            "errors": report.errors,
            "device": {
                "serial": "<serial>" if device else "",
                "state": device.state if device else "",
            },
            "collectedAt": report.collected_at,
        }

    def diagnostics_export(self, params: dict[str, Any]) -> dict[str, Any]:
        if self._report is None:
            raise RuntimeError("请先采集诊断信息")
        directory = params.get("directory")
        path = export_report(self._report, directory=Path(str(directory)) if directory else None)
        self.emit_log("DIAG", f"已导出诊断包：{path.name}")
        return {"path": str(path)}

    def diagnostics_static(self, _params: dict[str, Any]) -> dict[str, Any]:
        return {"text": static_summary()}

    # ------------------------------------------------------------------ 系统 / 向导 / 日志
    def system_paths(self, _params: dict[str, Any]) -> dict[str, Any]:
        return {
            "configPath": str(self.path),
            "logDir": str(C.log_dir()),
            "noticesPath": str(C.APP_DIR / "THIRD_PARTY_NOTICES.md"),
            "version": C.VERSION,
            "license": C.LICENSE,
            "appName": C.APP_NAME,
        }

    def wizard_check(self, params: dict[str, Any]) -> dict[str, Any]:
        step = int(params.get("step") or 0)
        if step == 0:
            code, stdout, stderr = self._run_adb(("devices", "-l"))
            serials = [line.split()[0] for line in stdout.splitlines() if "\tdevice" in line]
            return {
                "ok": code == 0 and bool(serials),
                "text": f"已连接：{', '.join(serials)}"
                if serials
                else (stderr or "未检测到设备，请检查数据线与 USB 调试。"),
            }
        if step == 1:
            code, stdout, _ = self._run_adb(("shell", "settings", "get", "global", "adb_enabled"))
            _, devices, _ = self._run_adb(("devices", "-l"))
            unauthorized = "unauthorized" in devices
            text = (
                "设备显示 unauthorized：请在平板上点「允许 USB 调试」。"
                if unauthorized
                else (
                    "USB 调试已开启。"
                    if code == 0 and stdout.strip() == "1"
                    else "请确认开发者选项里的 USB 调试已开启。"
                )
            )
            return {"ok": not unauthorized, "text": text}
        if step == 2:
            _, group, _ = self._run_adb(("shell", "id"))
            _, opened, _ = self._run_adb(("shell", "exec 3<>/dev/uhid && echo OPEN_OK"))
            has_group = "uhid" in group
            can_open = "OPEN_OK" in opened
            text = "\n".join(
                [
                    f"uhid 组：{'有' if has_group else '无'}",
                    f"/dev/uhid 可打开：{'是' if can_open else '否'}",
                    f"相对鼠标注入{'可用' if can_open else '不可用'}。",
                ]
            )
            return {"ok": can_open, "text": text}
        if step == 3:
            jar = Path(C.SCRCPY_SERVER_JAR)
            present = jar.is_file()
            size = jar.stat().st_size // 1024 if present else 0
            return {
                "ok": present,
                "text": f"{jar.name}（{size} KB）已就位。" if present else f"缺少 {jar.name}",
            }
        return {"ok": True, "text": "一切就绪，回到主窗口开始使用。"}

    def wizard_push_jar(self, _params: dict[str, Any]) -> dict[str, Any]:
        adb = getattr(self.runtime, "adb", None)
        if self.simulate or adb is None:
            return {"ok": True, "text": f"已推送到 {C.SCRCPY_REMOTE_JAR}（演示模式）"}
        try:
            adb.push(Path(C.SCRCPY_SERVER_JAR), "/data/local/tmp/scrcpy-server.jar")
        except Exception as exc:  # noqa: BLE001
            return {"ok": False, "text": f"推送失败：{exc}"}
        return {"ok": True, "text": "已推送到 /data/local/tmp/scrcpy-server.jar"}

    def wizard_finish(self, params: dict[str, Any]) -> dict[str, Any]:
        with self._lock:
            if "autoConnect" in params:
                self.cfg.device.auto_connect = bool(params["autoConnect"])
            if "closeToTray" in params:
                self.cfg.ui.close_to_tray = bool(params["closeToTray"])
            self.cfg.save(self.path)
        self.emit("config", {"config": config_dict(self.cfg), "issues": []})
        self.emit_log("SETUP", "首次设置完成")
        return {"config": config_dict(self.cfg)}

    def logs_recent(self, params: dict[str, Any]) -> dict[str, Any]:
        limit = max(1, min(1000, int(params.get("limit") or 200)))
        return {"logs": list(self._logs)[-limit:]}
