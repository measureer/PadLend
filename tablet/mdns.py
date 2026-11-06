"""直接用 mDNS 找 adb 无线调试端口（1.0.2）。

Windows 上的 adb 不一定带 mDNS 守护进程：``adb mdns check`` 会返回
"mdns daemon unavailable"，``adb mdns services`` 只有表头。平板自己仍然会
在 5353 端口广播 ``_adb-tls-connect._tcp.local``，所以这里用标准库直接发
一次 DNS 查询、解析响应，拿到当前的随机端口。

只依赖 :mod:`socket` / :mod:`struct`，不需要 Bonjour 或 zeroconf。解析函数
是纯函数，方便单测；真正的网络查询集中在 :func:`discover`。
"""

from __future__ import annotations

import contextlib
import logging
import socket
import struct
import time
from dataclasses import dataclass

log = logging.getLogger("tablet.mdns")

SERVICE_TYPE = "_adb-tls-connect._tcp.local"
MDNS_GROUP = "224.0.0.251"
MDNS_PORT = 5353

_TYPE_A = 1
_TYPE_PTR = 12
_TYPE_TXT = 16
_TYPE_SRV = 33
_CLASS_IN = 1
_CLASS_QU = 0x8000
_MAX_RECORDS = 256

_DEFAULT_TIMEOUT = 1.2


@dataclass(frozen=True, slots=True)
class MdnsService:
    """一条 adb 无线调试服务广播。"""

    ip: str
    port: int
    name: str = ""
    target: str = ""
    serial: str = ""
    model: str = ""


def _encode_name(name: str) -> bytes:
    parts = [part for part in name.strip(".").split(".") if part]
    out = bytearray()
    for part in parts:
        raw = part.encode("ascii")
        if not 0 < len(raw) < 64:
            raise ValueError(f"mDNS 名称片段长度非法: {part!r}")
        out.append(len(raw))
        out.extend(raw)
    out.append(0)
    if len(out) > 255:
        raise ValueError("mDNS 名称过长")
    return bytes(out)


def build_query(service: str = SERVICE_TYPE, qclass: int = _CLASS_IN) -> bytes:
    """构造一个单问题 DNS 查询；``qclass`` 带 QU 位时请求单播响应。"""
    header = struct.pack("!6H", 0, 0, 1, 0, 0, 0)
    return header + _encode_name(service) + struct.pack("!HH", _TYPE_PTR, qclass)


def _read_name(data: bytes, offset: int) -> tuple[str, int]:
    """读 DNS 名称，支持压缩指针；返回 (名称, 下一个记录偏移)。"""
    labels: list[str] = []
    next_offset: int | None = None
    jumps = 0
    while True:
        if offset < 0 or offset >= len(data):
            raise ValueError("DNS 名称越界")
        length = data[offset]
        if length == 0:
            offset += 1
            if next_offset is None:
                next_offset = offset
            return ".".join(labels), next_offset
        if length & 0xC0 == 0xC0:
            if offset + 1 >= len(data):
                raise ValueError("DNS 压缩指针越界")
            pointer = ((length & 0x3F) << 8) | data[offset + 1]
            if next_offset is None:
                next_offset = offset + 2
            jumps += 1
            if jumps > 16:
                raise ValueError("DNS 压缩指针成环")
            offset = pointer
            continue
        if length & 0xC0:
            raise ValueError("不支持的 DNS 标签类型")
        start = offset + 1
        end = start + length
        if end > len(data):
            raise ValueError("DNS 标签越界")
        labels.append(data[start:end].decode("utf-8", "replace"))
        if len(labels) > 63:
            raise ValueError("DNS 名称标签过多")
        offset = end


def _parse_txt(rdata: bytes) -> dict[str, str]:
    values: dict[str, str] = {}
    pos = 0
    while pos < len(rdata):
        length = rdata[pos]
        pos += 1
        if length == 0 or pos + length > len(rdata):
            continue
        text = rdata[pos : pos + length].decode("utf-8", "replace")
        pos += length
        key, sep, value = text.partition("=")
        if key and sep and key not in values:
            values[key] = value
    return values


def _is_connect_service(name: str) -> bool:
    return name.lower().rstrip(".").endswith(SERVICE_TYPE)


def parse_response(data: bytes, source_ip: str = "") -> list[MdnsService]:
    """解析一个 mDNS 响应；坏包返回空列表，绝不抛给调用方。"""
    if len(data) < 12:
        return []
    try:
        _ident, flags, qdcount, ancount, nscount, arcount = struct.unpack(
            "!6H", data[:12]
        )
    except struct.error:
        return []
    if not flags & 0x8000:
        return []

    offset = 12
    try:
        for _ in range(min(qdcount, 64)):
            _name, offset = _read_name(data, offset)
            offset += 4  # QTYPE + QCLASS
    except (ValueError, IndexError):
        return []

    records: list[tuple[str, int, int, int]] = []
    total = min(ancount + nscount + arcount, _MAX_RECORDS)
    for _ in range(total):
        try:
            name, offset = _read_name(data, offset)
        except (ValueError, IndexError):
            break
        if offset + 10 > len(data):
            break
        try:
            rtype, _rclass, _ttl, rdlength = struct.unpack("!HHIH", data[offset : offset + 10])
        except struct.error:
            break
        offset += 10
        if offset + rdlength > len(data):
            break
        records.append((name, rtype, offset, rdlength))
        offset += rdlength

    addresses: dict[str, str] = {}
    services: dict[str, tuple[int, str]] = {}
    text: dict[str, dict[str, str]] = {}
    for name, rtype, rdata, rdlength in records:
        key = name.lower().rstrip(".")
        if rtype == _TYPE_A and rdlength >= 4:
            addresses[key] = socket.inet_ntoa(data[rdata : rdata + 4])
        elif rtype == _TYPE_SRV and rdlength >= 6:
            try:
                _priority, _weight, port = struct.unpack("!HHH", data[rdata : rdata + 6])
                target, _ = _read_name(data, rdata + 6)
            except (ValueError, struct.error):
                continue
            services[key] = (port, target)
        elif rtype == _TYPE_TXT:
            text[key] = _parse_txt(data[rdata : rdata + rdlength])

    found: list[MdnsService] = []
    seen: set[tuple[str, int]] = set()
    for name, (port, target) in services.items():
        if not _is_connect_service(name) or not 0 < port < 65536:
            continue
        ip = addresses.get(target.lower().rstrip("."), "")
        if not ip:
            ip = source_ip.strip()
        if not ip:
            continue
        pair = (ip, port)
        if pair in seen:
            continue
        seen.add(pair)
        info = text.get(name, {})
        found.append(
            MdnsService(
                ip=ip,
                port=port,
                name=name,
                target=target,
                serial=info.get("serial", ""),
                model=info.get("model", ""),
            )
        )
    return found


def discover(
    timeout: float = _DEFAULT_TIMEOUT,
    *,
    service: str = SERVICE_TYPE,
) -> list[MdnsService]:
    """发一次 mDNS 查询，收集 ``timeout`` 秒内的响应。

    5353 能绑定就加入组播组；绑不上（被 Bonjour 等占用）时改用临时端口并
    在查询里置 QU 位，请求对方单播回应。网络不可用时返回空列表。
    """
    budget = max(0.1, float(timeout))
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        bound = False
        try:
            sock.bind(("", MDNS_PORT))
            bound = True
        except OSError:
            sock.bind(("", 0))
        sock.settimeout(0.2)

        joined = False
        if bound:
            try:
                membership = socket.inet_aton(MDNS_GROUP) + socket.inet_aton(
                    "0.0.0.0"
                )
                sock.setsockopt(socket.IPPROTO_IP, socket.IP_ADD_MEMBERSHIP, membership)
                joined = True
            except OSError as exc:
                log.debug("mDNS 组播加入失败: %r", exc)
        with contextlib.suppress(OSError):
            sock.setsockopt(
                socket.IPPROTO_IP, socket.IP_MULTICAST_TTL, struct.pack("b", 1)
            )

        qclass = _CLASS_IN if joined else (_CLASS_IN | _CLASS_QU)
        query = build_query(service, qclass)
        deadline = time.monotonic() + budget
        next_send = 0.0
        found: list[MdnsService] = []
        seen: set[tuple[str, int]] = set()
        while True:
            now = time.monotonic()
            if now >= deadline:
                break
            if now >= next_send:
                try:
                    sock.sendto(query, (MDNS_GROUP, MDNS_PORT))
                except OSError as exc:
                    log.debug("mDNS 查询发送失败: %r", exc)
                    break
                next_send = now + 0.35
            try:
                data, address = sock.recvfrom(9000)
            except TimeoutError:
                continue
            except OSError:
                break
            try:
                for item in parse_response(data, address[0]):
                    pair = (item.ip, item.port)
                    if pair not in seen:
                        seen.add(pair)
                        found.append(item)
            except Exception as exc:  # noqa: BLE001 - 单个坏包不影响后续
                log.debug("mDNS 响应解析失败: %r", exc)
        if found:
            log.info(
                "mDNS 找到 %d 个 adb 服务: %s",
                len(found),
                ", ".join(f"{item.ip}:{item.port}" for item in found),
            )
        return found
    except OSError as exc:
        log.debug("mDNS 查询不可用: %r", exc)
        return []
    finally:
        sock.close()
