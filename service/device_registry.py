"""设备注册表：把设备身份和连接端点分开保存。

以前设备记忆只有 config.yaml 里的 device.known（一串 adb serial）和
device.macs。无线调试的 serial 是 IP:端口：路由器重新分配 IP、平板重启
后端口变化，同一台平板就会变成另一台设备。本模块用独立 devices.json
记录稳定身份（优先硬件序列号）与它历史用过的所有端点，网络怎么变都只
对应一台设备。

设计约束：
* 纯数据模块，不调用 adb、不访问网络，便于单元测试；
* 文件原子写；损坏时备份为 devices.json.bad-<时间> 后从空表重建；
* 不同硬件序列号即使 MAC 相同也绝不合并（Android 10+ 的随机 MAC）；
* 没有硬件序列号时先建 ep:<端点> 临时记录，拿到稳定身份后原地升级，
  同时保留原端点和 MAC，供迁移布局与后续找回。
"""

from __future__ import annotations

import contextlib
import json
import logging
import os
import re
import shutil
import tempfile
from dataclasses import dataclass, field
from datetime import UTC, datetime
from pathlib import Path
from typing import Any

log = logging.getLogger("service.device_registry")

SCHEMA_VERSION = 1
MAX_DEVICES = 32
MAX_ENDPOINTS = 8
MAX_MACS = 8

_MAC_RE = re.compile(r"^[0-9a-f]{2}(?::[0-9a-f]{2}){5}$")
_INVALID_IDS = {"", "unknown", "null", "0", "none", "n/a"}
_VOLATILE_SUFFIXES = ("_adb-tls-connect._tcp", "_adb-tls-pairing._tcp", ".tcp")


def normalize_serial(serial: Any) -> str:
    """adb serial / 端点串归一化：只去空白，不改大小写（USB 序列号区分大小写）。"""
    return str(serial or "").strip()


def normalize_mac(mac: Any) -> str:
    """统一成小写 aa:bb:cc:dd:ee:ff；非法值返回空串。"""
    text = str(mac or "").strip().lower().replace("-", ":")
    return text if _MAC_RE.fullmatch(text) else ""


def is_wireless_serial(serial: str) -> bool:
    """是否无线端点：ip:port、mDNS 服务名或 host:port。"""
    text = normalize_serial(serial)
    if not text:
        return False
    if ":" in text:
        return True
    return text.lower().endswith(_VOLATILE_SUFFIXES)


def is_volatile_serial(serial: str) -> bool:
    """这个值会不会随网络/连接方式变化；是则不能当设备身份。"""
    return is_wireless_serial(serial)


def canonical_hardware_serial(hardware_serial: Any, endpoint_serial: Any) -> str:
    """选出可作稳定身份的硬件序列号；没有稳定值时返回空串。

    tablet.session.read_facts 在 ro.serialno 无效时会回退成 adb serial。
    无线 adb serial 是 IP:端口，拿它当身份会让换网后的同一台平板分裂成
    两台；这里显式拒绝“硬件序列号等于无线端点”的伪稳定值。
    """
    value = normalize_serial(hardware_serial)
    if value.lower() in _INVALID_IDS:
        return ""
    if is_wireless_serial(value):
        return ""
    if normalize_serial(endpoint_serial) == value and is_wireless_serial(endpoint_serial):
        return ""
    return value


def split_endpoint(serial: str) -> tuple[str, int, str]:
    """拆出 (host, port, transport)，支持 IPv4、主机名和 [IPv6]:port。

    transport 为 wifi 或 usb；无法解析端口时 port=0，但主机名
    （mDNS 的 adb-xxx._adb-tls-connect._tcp）仍按无线处理。
    """
    text = normalize_serial(serial)
    if not text:
        return "", 0, ""
    if text.startswith("["):
        host, sep, rest = text[1:].partition("]")
        if not sep or not host:
            return "", 0, "wifi"
        port = 0
        if rest.startswith(":"):
            digits = rest[1:]
            if digits.isdigit():
                port = int(digits)
        return host, port, "wifi"
    if text.count(":") == 1:
        host, _, digits = text.partition(":")
        if host and digits.isdigit():
            port = int(digits)
            if 0 < port < 65536:
                return host, port, "wifi"
        return host, 0, "wifi"
    if text.count(":") > 1:
        # 未加方括号的 IPv6：没有端口可拆，整串作为 host。
        return text, 0, "wifi"
    return text, 0, "usb" if not is_wireless_serial(text) else "wifi"


def endpoint_label(serial: str) -> str:
    host, port, transport = split_endpoint(serial)
    if transport == "usb":
        return "USB"
    return f"{host}:{port}" if port else host


def _now_text(now: datetime | None = None) -> str:
    stamp = now or datetime.now(UTC)
    if stamp.tzinfo is None:
        stamp = stamp.replace(tzinfo=UTC)
    return stamp.astimezone(UTC).isoformat(timespec="seconds")


@dataclass(slots=True)
class Endpoint:
    serial: str
    host: str = ""
    port: int = 0
    transport: str = "usb"
    source: str = ""
    label: str = ""
    aliases: list[str] = field(default_factory=list)
    last_ok: str = ""
    last_seen: str = ""
    fail_count: int = 0
    last_error: str = ""

    @classmethod
    def from_dict(cls, raw: Any) -> Endpoint | None:
        if not isinstance(raw, dict):
            return None
        serial = normalize_serial(raw.get("serial"))
        if not serial:
            return None
        host, parsed_port, parsed_transport = split_endpoint(serial)
        try:
            port = int(raw.get("port") or parsed_port or 0)
        except (TypeError, ValueError):
            port = parsed_port
        transport = str(raw.get("transport") or parsed_transport)
        if transport not in ("usb", "wifi"):
            transport = parsed_transport
        aliases = [
            normalize_serial(item)
            for item in (raw.get("aliases") or [])
            if normalize_serial(item)
        ]
        try:
            fail_count = max(0, int(raw.get("fail_count") or 0))
        except (TypeError, ValueError):
            fail_count = 0
        return cls(
            serial=serial,
            host=str(raw.get("host") or host),
            port=port if 0 <= port < 65536 else parsed_port,
            transport=transport,
            source=str(raw.get("source") or ""),
            label=str(raw.get("label") or ""),
            aliases=list(dict.fromkeys(aliases))[:MAX_ENDPOINTS],
            last_ok=str(raw.get("last_ok") or ""),
            last_seen=str(raw.get("last_seen") or ""),
            fail_count=fail_count,
            last_error=str(raw.get("last_error") or ""),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "serial": self.serial,
            "host": self.host,
            "port": self.port,
            "transport": self.transport,
            "source": self.source,
            "label": self.label,
            "aliases": list(self.aliases),
            "last_ok": self.last_ok,
            "last_seen": self.last_seen,
            "fail_count": self.fail_count,
            "last_error": self.last_error,
        }


@dataclass(slots=True)
class DeviceRecord:
    id: str
    hardware_serial: str = ""
    model: str = ""
    android_id: str = ""
    macs: list[str] = field(default_factory=list)
    endpoints: list[Endpoint] = field(default_factory=list)
    first_seen: str = ""
    last_seen: str = ""

    @classmethod
    def from_dict(cls, raw: Any) -> DeviceRecord | None:
        if not isinstance(raw, dict):
            return None
        record_id = str(raw.get("id") or "").strip()
        hardware = str(raw.get("hardware_serial") or "").strip()
        if not record_id and hardware:
            record_id = f"hw:{hardware.casefold()}"
        if not record_id:
            return None
        endpoints: list[Endpoint] = []
        seen: set[str] = set()
        for item in raw.get("endpoints") or []:
            endpoint = Endpoint.from_dict(item)
            if endpoint is None or endpoint.serial in seen:
                continue
            seen.add(endpoint.serial)
            endpoints.append(endpoint)
        macs = [
            normalize_mac(item)
            for item in (raw.get("macs") or [])
            if normalize_mac(item)
        ]
        return cls(
            id=record_id,
            hardware_serial=hardware,
            model=str(raw.get("model") or ""),
            android_id=str(raw.get("android_id") or ""),
            macs=list(dict.fromkeys(macs))[:MAX_MACS],
            endpoints=endpoints[:MAX_ENDPOINTS],
            first_seen=str(raw.get("first_seen") or ""),
            last_seen=str(raw.get("last_seen") or ""),
        )

    def to_dict(self) -> dict[str, Any]:
        return {
            "id": self.id,
            "hardware_serial": self.hardware_serial,
            "model": self.model,
            "android_id": self.android_id,
            "macs": list(self.macs),
            "endpoints": [endpoint.to_dict() for endpoint in self.endpoints],
            "first_seen": self.first_seen,
            "last_seen": self.last_seen,
        }

    def endpoint(self, serial: str) -> Endpoint | None:
        text = normalize_serial(serial)
        for endpoint in self.endpoints:
            if endpoint.serial == text or text in endpoint.aliases:
                return endpoint
        folded = text.casefold()
        for endpoint in self.endpoints:
            if endpoint.serial.casefold() == folded:
                return endpoint
        return None

    def primary_serial(self, current: str = "") -> str:
        """列表/自动连接优先用的端点：当前端点 → 最近成功 → 最近看到 → 第一个。"""
        chosen = normalize_serial(current)
        if chosen and self.endpoint(chosen) is not None:
            return chosen
        ok = [endpoint for endpoint in self.endpoints if endpoint.last_ok]
        if ok:
            return max(ok, key=lambda endpoint: (endpoint.last_ok, endpoint.last_seen)).serial
        seen = [endpoint for endpoint in self.endpoints if endpoint.last_seen]
        if seen:
            return max(seen, key=lambda endpoint: endpoint.last_seen).serial
        return self.endpoints[0].serial if self.endpoints else ""


class DeviceRegistry:
    """devices.json 的读写与身份合并；所有公开方法都不抛 I/O 异常。"""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)
        self.devices: list[DeviceRecord] = []
        self.issues: list[str] = []
        self.existed = False
        self.needs_rebuild = False

    # -- 序列化 ------------------------------------------------------------
    def to_dict(self) -> dict[str, Any]:
        return {
            "version": SCHEMA_VERSION,
            "devices": [record.to_dict() for record in self.devices],
        }

    def load(self) -> DeviceRegistry:
        """读文件；不存在返回空表，损坏则备份后从空表重建（绝不阻断启动）。"""
        self.devices = []
        self.issues = []
        self.existed = self.path.exists()
        self.needs_rebuild = False
        if not self.existed:
            return self
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
            log.warning("devices.json 读取失败: %r", exc)
            self.needs_rebuild = True
            backup = self._backup(reason="read")
            self.issues.append(
                "设备记录损坏，已从空表重建"
                + (f"（备份为 {backup.name}）" if backup is not None else "")
            )
            return self
        if not isinstance(raw, dict):
            self.needs_rebuild = True
            backup = self._backup(reason="not-object")
            self.issues.append(
                "设备记录格式不正确，已从空表重建"
                + (f"（备份为 {backup.name}）" if backup is not None else "")
            )
            return self
        version = raw.get("version", SCHEMA_VERSION)
        try:
            if int(version) > SCHEMA_VERSION:
                backup = self._backup(reason="future-version")
                self.issues.append(
                    f"设备记录版本 {version} 高于当前程序"
                    + (f"（备份为 {backup.name}）" if backup is not None else "")
                )
                return self
        except (TypeError, ValueError):
            log.warning("devices.json 版本非法: %r", version)
        seen: set[str] = set()
        for item in raw.get("devices") or []:
            record = DeviceRecord.from_dict(item)
            if record is None or record.id in seen or not record.endpoints:
                continue
            seen.add(record.id)
            self.devices.append(record)
            if len(self.devices) >= MAX_DEVICES:
                break
        return self

    def _backup(self, *, reason: str) -> Path | None:
        stamp = datetime.now().strftime("%Y%m%d-%H%M%S")
        target = self.path.with_name(f"{self.path.name}.bad-{stamp}")
        try:
            shutil.copy2(self.path, target)
            log.warning("devices.json 已备份 path=%s reason=%s", target, reason)
            return target
        except OSError as exc:  # pragma: no cover - 文件系统异常
            log.error("devices.json 备份失败: %r", exc)
            return None

    def save(self) -> bool:
        """原子写入；失败只告警并返回 False（设备记忆不能拖垮连接）。"""
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            payload = json.dumps(self.to_dict(), ensure_ascii=False, indent=2) + "\n"
            fd, tmp = tempfile.mkstemp(
                prefix=f"{self.path.name}.", suffix=".tmp", dir=str(self.path.parent)
            )
            try:
                with os.fdopen(fd, "w", encoding="utf-8", newline="\n") as handle:
                    handle.write(payload)
                    handle.flush()
                    os.fsync(handle.fileno())
                os.replace(tmp, self.path)
            finally:
                if os.path.exists(tmp):
                    with contextlib.suppress(OSError):
                        os.unlink(tmp)
            self.existed = True
            self.needs_rebuild = False
            return True
        except OSError as exc:
            log.warning("devices.json 保存失败: %r", exc)
            return False

    # -- 查询 --------------------------------------------------------------
    def records(self) -> tuple[DeviceRecord, ...]:
        return tuple(self.devices)

    def find(self, record_id: str) -> DeviceRecord | None:
        text = str(record_id or "")
        for record in self.devices:
            if record.id == text:
                return record
        return None

    def find_by_serial(self, serial: str) -> DeviceRecord | None:
        text = normalize_serial(serial)
        if not text:
            return None
        for record in self.devices:
            if record.endpoint(text) is not None:
                return record
        folded = text.casefold()
        for record in self.devices:
            for endpoint in record.endpoints:
                if endpoint.serial.casefold() == folded:
                    return record
        return None

    def mark_failure(self, serial: str, detail: str, *, now: datetime | None = None) -> bool:
        """记录一次端点连接失败；成功连接仍由 upsert() 清零。

        只更新端点，不改 last_ok/last_seen（它们表示最后一次成功）。
        失败信息用于设备页解释“为什么这个地址没连上”，并给候选排序提供依据。
        """
        text_serial = normalize_serial(serial)
        record = self.find_by_serial(text_serial)
        if record is None:
            return False
        endpoint = record.endpoint(text_serial)
        if endpoint is None:
            return False
        endpoint.fail_count = min(endpoint.fail_count + 1, 9999)
        endpoint.last_error = str(detail or "")[:300]
        return True

    def find_by_hardware(self, hardware_serial: str) -> DeviceRecord | None:
        text = normalize_serial(hardware_serial).casefold()
        if not text:
            return None
        for record in self.devices:
            if record.hardware_serial and record.hardware_serial.casefold() == text:
                return record
        return None

    def find_by_mac(self, mac: str) -> DeviceRecord | None:
        wanted = normalize_mac(mac)
        if not wanted:
            return None
        for record in self.devices:
            if wanted in record.macs:
                return record
        return None

    def serials(self, current: str = "") -> list[str]:
        """去重后的端点列表，当前设备在最前；用于“已记住设备”与自动连接。"""
        out: list[str] = []
        chosen = normalize_serial(current)
        if chosen and self.find_by_serial(chosen) is not None:
            out.append(chosen)
        for record in self._ordered_records(chosen):
            serial = record.primary_serial(chosen)
            if serial and serial not in out:
                out.append(serial)
        return out

    def all_serials(self) -> list[str]:
        """所有历史端点（无线查找用；一台设备可能有多个地址）。"""
        out: list[str] = []
        for record in self.devices:
            for endpoint in record.endpoints:
                if endpoint.serial not in out:
                    out.append(endpoint.serial)
        return out

    def _ordered_records(self, current: str = "") -> list[DeviceRecord]:
        chosen = normalize_serial(current)
        owner = self.find_by_serial(chosen) if chosen else None

        def key(record: DeviceRecord) -> tuple[int, str, str]:
            return (
                1 if owner is record else 0,
                record.last_seen,
                record.primary_serial(chosen),
            )

        return sorted(self.devices, key=key, reverse=True)

    # -- 写入 --------------------------------------------------------------
    def upsert(
        self,
        serial: str,
        *,
        hardware_serial: str = "",
        model: str = "",
        mac: str = "",
        android_id: str = "",
        source: str = "",
        now: datetime | None = None,
    ) -> DeviceRecord | None:
        """把一次成功连接写入注册表；返回记录，输入为空时返回 None。

        合并规则：同一硬件序列号一定合并；没有硬件序列号的临时记录可按
        端点或 MAC 合并；两个都已知硬件序列号的记录绝不因 MAC 相同而合并。
        """
        endpoint_serial = normalize_serial(serial)
        if not endpoint_serial:
            return None
        stamp = _now_text(now)
        hardware = canonical_hardware_serial(hardware_serial, endpoint_serial)
        clean_mac = normalize_mac(mac)
        by_serial = self.find_by_serial(endpoint_serial)
        by_hw = self.find_by_hardware(hardware) if hardware else None
        by_mac = self.find_by_mac(clean_mac) if clean_mac else None

        target: DeviceRecord | None
        if by_hw is not None:
            target = by_hw
        elif by_serial is not None:
            target = by_serial
        elif by_mac is not None:
            target = by_mac
        else:
            target = DeviceRecord(
                id=self._record_id(hardware, endpoint_serial),
                first_seen=stamp,
            )
            self.devices.append(target)

        # 目标记录还没有硬件序列号时升级；已有不同硬件序列号则另立记录。
        if (
            hardware
            and target.hardware_serial
            and target.hardware_serial.casefold() != hardware.casefold()
        ):
            target = DeviceRecord(
                id=self._record_id(hardware, endpoint_serial),
                first_seen=stamp,
            )
            self.devices.append(target)
        if hardware and not target.hardware_serial:
            target.hardware_serial = hardware
        if hardware:
            target.id = f"hw:{hardware.casefold()}"

        # 合并可证明属于同一台设备的记录：同一硬件序列号；或目标已有
        # 硬件序列号时，按 MAC / 端点吸收临时记录。两个硬件序列号不同
        # 的记录绝不合并（随机 MAC 会让不同设备撞 MAC）。
        for record in list(self.devices):
            if record is target:
                continue
            same_hw = bool(
                hardware
                and record.hardware_serial
                and record.hardware_serial.casefold() == hardware.casefold()
            )
            provisional = not record.hardware_serial
            same_mac = bool(clean_mac and clean_mac in record.macs)
            same_endpoint = record.endpoint(endpoint_serial) is not None
            if same_hw or (provisional and (same_mac or same_endpoint)):
                self._merge(target, record)

        # 端点被另一个记录占用：同一地址可能被另一台平板复用，现在把
        # 端点迁到目标记录，不能因为地址相同而合并两台设备。
        for record in list(self.devices):
            if record is target:
                continue
            endpoint = record.endpoint(endpoint_serial)
            if endpoint is None:
                continue
            record.endpoints = [item for item in record.endpoints if item is not endpoint]
            self._drop_if_empty(record)

        endpoint = target.endpoint(endpoint_serial)
        if endpoint is None:
            host, port, transport = split_endpoint(endpoint_serial)
            endpoint = Endpoint(
                serial=endpoint_serial,
                host=host,
                port=port,
                transport=transport,
                source=source or "connect",
                label=endpoint_label(endpoint_serial),
            )
        elif source:
            endpoint.source = source
        target.endpoints = [item for item in target.endpoints if item is not endpoint]
        target.endpoints.insert(0, endpoint)
        target.endpoints = target.endpoints[:MAX_ENDPOINTS]
        target.last_seen = stamp
        endpoint.last_seen = stamp
        endpoint.last_ok = stamp
        endpoint.fail_count = 0
        endpoint.last_error = ""
        if model.strip():
            target.model = model.strip()
        if android_id.strip():
            target.android_id = android_id.strip()
        if clean_mac and clean_mac not in target.macs:
            target.macs.append(clean_mac)
        target.macs = target.macs[:MAX_MACS]
        self._trim()
        return target

    def rename_endpoint(
        self,
        previous: str,
        target: str,
        *,
        mac: str = "",
        source: str = "locate",
        now: datetime | None = None,
    ) -> DeviceRecord | None:
        """找回设备后把旧端点改名为新地址，保留记录与历史端点。"""
        old = normalize_serial(previous)
        new = normalize_serial(target)
        if not old or not new:
            return None
        record = self.find_by_serial(old)
        if record is None:
            return None
        if old == new:
            endpoint = record.endpoint(old)
            if endpoint is not None:
                endpoint.last_ok = _now_text(now)
            return record
        other = self.find_by_serial(new)
        if other is not None and other is not record:
            # 新地址已经属于另一台设备：不偷端点，交给调用方按离线处理。
            return None
        stamp = _now_text(now)
        old_endpoint = record.endpoint(old)
        if old_endpoint is not None:
            source_name = old_endpoint.source
            label = old_endpoint.label
            aliases = old_endpoint.aliases
            record.endpoints = [
                item for item in record.endpoints if item is not old_endpoint
            ]
        else:
            source_name, label, aliases = source, endpoint_label(new), []
        host, port, transport = split_endpoint(new)
        record.endpoints.insert(
            0,
            Endpoint(
                serial=new,
                host=host,
                port=port,
                transport=transport,
                source=source_name or source,
                label=label or endpoint_label(new),
                aliases=list(dict.fromkeys([*aliases, old]))[:MAX_ENDPOINTS],
                last_ok=stamp,
                last_seen=stamp,
            ),
        )
        record.endpoints = record.endpoints[:MAX_ENDPOINTS]
        record.last_seen = stamp
        clean_mac = normalize_mac(mac)
        if clean_mac and clean_mac not in record.macs:
            record.macs.append(clean_mac)
        return record

    def remove(self, serial: str) -> bool:
        """从注册表移除一个端点；记录没有端点时一并删除。"""
        text = normalize_serial(serial)
        record = self.find_by_serial(text)
        if record is None:
            return False
        endpoint = record.endpoint(text)
        if endpoint is None:
            return False
        record.endpoints = [item for item in record.endpoints if item is not endpoint]
        self._drop_if_empty(record)
        return True

    def import_legacy(
        self,
        *,
        current: str = "",
        known: list[str] | None = None,
        macs: dict[str, str] | None = None,
        now: datetime | None = None,
    ) -> int:
        """把旧 config.yaml 的 known/macs 一次性迁成临时设备记录。

        只按有记录的 MAC 分组；没有 MAC 的端点各成一个记录。这里不猜测
        硬件序列号，等设备下次连接时由 upsert 原地升级。
        """
        stamp = _now_text(now)
        serials: list[str] = []
        for item in [current, *(known or [])]:
            text = normalize_serial(item)
            if text and text not in serials:
                serials.append(text)
        if not serials:
            return 0
        by_mac: dict[str, DeviceRecord] = {}
        imported: dict[str, DeviceRecord] = {}
        for serial in serials:
            clean_mac = normalize_mac((macs or {}).get(serial, ""))
            record = by_mac.get(clean_mac) if clean_mac else None
            if record is None:
                record = DeviceRecord(
                    id=f"ep:{serial.casefold()}",
                    first_seen=stamp,
                )
                self.devices.append(record)
                if clean_mac:
                    by_mac[clean_mac] = record
            if clean_mac and clean_mac not in record.macs:
                record.macs.append(clean_mac)
            if record.endpoint(serial) is None:
                host, port, transport = split_endpoint(serial)
                record.endpoints.append(
                    Endpoint(
                        serial=serial,
                        host=host,
                        port=port,
                        transport=transport,
                        source="legacy",
                        label=endpoint_label(serial),
                    )
                )
            imported[serial] = record
        for record in imported.values():
            record.endpoints = record.endpoints[:MAX_ENDPOINTS]
            record.macs = record.macs[:MAX_MACS]
        self._trim()
        return len(imported)

    # -- 内部 --------------------------------------------------------------
    @staticmethod
    def _record_id(hardware: str, serial: str) -> str:
        if hardware:
            return f"hw:{hardware.casefold()}"
        return f"ep:{normalize_serial(serial).casefold()}"

    def _merge(self, target: DeviceRecord, source: DeviceRecord) -> None:
        if source is target or source not in self.devices:
            return
        for endpoint in source.endpoints:
            if target.endpoint(endpoint.serial) is None:
                target.endpoints.append(endpoint)
        for mac in source.macs:
            if mac not in target.macs:
                target.macs.append(mac)
        if not target.model and source.model:
            target.model = source.model
        if not target.android_id and source.android_id:
            target.android_id = source.android_id
        if not target.first_seen or (
            source.first_seen and source.first_seen < target.first_seen
        ):
            target.first_seen = source.first_seen
        if source.last_seen > target.last_seen:
            target.last_seen = source.last_seen
        target.endpoints = target.endpoints[:MAX_ENDPOINTS]
        target.macs = target.macs[:MAX_MACS]
        self.devices.remove(source)

    def _drop_if_empty(self, record: DeviceRecord) -> None:
        if not record.endpoints and record in self.devices:
            self.devices.remove(record)

    def _trim(self) -> None:
        self.devices = self.devices[:MAX_DEVICES]
        for record in self.devices:
            record.endpoints = record.endpoints[:MAX_ENDPOINTS]
            record.macs = record.macs[:MAX_MACS]
