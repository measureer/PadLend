"""启动时查找上次连接的平板（0.3.13）。

无线调试的两个东西都会变：路由器可能给平板换了局域网 IP，平板重启后
「无线调试」的端口也会重新随机分配。只按上次的 ``ip:port`` 去连，经常会
明明在同一个 Wi-Fi 里却报「离线」。

这里的思路是三层递进：

1. 先用上次记下的地址做一次 TCP 连接（端口能连上基本就说明 adb 还在）；
2. 连不上就查 ``adb mdns services``——无线调试会通过 mDNS 广播当前端口，
   同一个 IP 换了端口也能找到新地址；
3. IP 也变了的话，用上次记下的 MAC 在系统 ARP 邻居表里找它现在挂在哪个
   地址上；找到 IP 后再回头查 mDNS 有没有对应端口。

三层都失败才算离线。全是纯函数 + 注入的 ``services()``，便于单测。
"""

from __future__ import annotations

import logging
import re
import socket
import subprocess
from collections.abc import Callable, Sequence
from dataclasses import dataclass

log = logging.getLogger("tablet.locator")

STATUS_ONLINE = "online"
STATUS_MOVED = "moved"
STATUS_NO_PORT = "no_port"
STATUS_OFFLINE = "offline"

_IPV4 = re.compile(r"(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}):(\d{1,5})")
_MAC = re.compile(r"^[0-9a-f]{2}(?::[0-9a-f]{2}){5}$")
_ARP_LINE = re.compile(
    r"(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3})\s+"
    r"([0-9a-fA-F]{2}(?:-[0-9a-fA-F]{2}){5})"
)


@dataclass(frozen=True, slots=True)
class Located:
    """一次查找的结论；``detail`` 是可以直接显示给用户的中文句子。"""

    status: str = STATUS_OFFLINE
    address: str = ""
    ip: str = ""
    port: int = 0
    mac: str = ""
    detail: str = ""

    @property
    def online(self) -> bool:
        return self.status == STATUS_ONLINE

    @property
    def reachable(self) -> bool:
        """至少 IP 上找到了，只是可能缺端口。"""
        return self.status in (STATUS_ONLINE, STATUS_MOVED, STATUS_NO_PORT)


def split_address(address: str) -> tuple[str, int]:
    """``"192.168.1.8:37495"`` → ``("192.168.1.8", 37495)``；无端口返回 5555。"""
    text = str(address or "").strip()
    if not text:
        return "", 0
    host, sep, port = text.partition(":")
    host = host.strip()
    if not host:
        return "", 0
    if not sep:
        return host, 5555
    number = port.strip()
    if not number.isdigit():
        return host, 5555
    value = int(number)
    if not 0 < value < 65536:
        return host, 5555
    return host, value


def normalize_mac(value: str | None) -> str:
    """统一成小写冒号分隔；``88-A4-79-...`` 与 ``88:a4:79:...`` 等价。"""
    text = str(value or "").strip().lower().replace("-", ":")
    text = re.sub(r":+", ":", text)
    return text if _MAC.match(text) else ""


def tcp_open(ip: str, port: int, *, timeout: float = 1.5) -> bool:
    """这个地址现在是否接受连接（不依赖 adb，避免触发 adb server 启动）。"""
    if not ip or not port:
        return False
    try:
        with socket.create_connection((ip, int(port)), timeout=timeout):
            return True
    except OSError:
        return False


def parse_mdns(lines: Sequence[str]) -> list[tuple[str, int]]:
    """从 ``adb mdns services`` 的输出里抽出 ``(ip, port)`` 列表。"""
    found: list[tuple[str, int]] = []
    for line in lines:
        for match in _IPV4.finditer(str(line)):
            host, port = match.group(1), int(match.group(2))
            if 0 < port < 65536 and (host, port) not in found:
                found.append((host, port))
    return found


def mdns_port_for(ip: str, services: Sequence[tuple[str, int]]) -> int:
    for host, port in services:
        if host == ip:
            return port
    return 0


def arp_table() -> dict[str, str]:
    """系统 ARP 邻居表：MAC（统一格式）→ IP。

    用 ``arp -a`` 而不是 PowerShell，避免启动慢和编码问题；输出按字节读，
    中文 Windows 的 GBK 页头也不会把正则搞乱。
    """
    try:
        proc = subprocess.run(
            ["arp", "-a"],
            capture_output=True,
            timeout=5.0,
            check=False,
            creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        )
    except Exception as exc:  # noqa: BLE001 - 查询失败就当没有邻居表
        log.debug("arp -a failed: %r", exc)
        return {}
    table: dict[str, str] = {}
    for raw_line in (proc.stdout or b"").splitlines():
        text = raw_line.decode("utf-8", "replace")
        match = _ARP_LINE.search(text)
        if not match:
            continue
        ip, mac = match.group(1), normalize_mac(match.group(2))
        if mac and ip not in ("0.0.0.0", "255.255.255.255"):
            table.setdefault(mac, ip)
    return table


def find_ip_by_mac(mac: str, table: dict[str, str]) -> str:
    wanted = normalize_mac(mac)
    return table.get(wanted, "") if wanted else ""


def locate(
    address: str,
    mac: str = "",
    *,
    services: Callable[[], Sequence[str]] | None = None,
    arp: Callable[[], dict[str, str]] | None = None,
    timeout: float = 1.5,
) -> Located:
    """按「原地址 → mDNS → MAC/ARP → 离线」的顺序找上次那台平板。"""
    host, port = split_address(address)
    if not host:
        return Located(status=STATUS_OFFLINE, mac=normalize_mac(mac), detail="还没有连接过设备")

    if tcp_open(host, port, timeout=timeout):
        return Located(
            status=STATUS_ONLINE,
            address=f"{host}:{port}",
            ip=host,
            port=port,
            mac=normalize_mac(mac),
            detail=f"上次的地址 {host}:{port} 仍然在线",
        )

    mdns: list[tuple[str, int]] = []
    if services is not None:
        try:
            mdns = parse_mdns(services())
        except Exception as exc:  # noqa: BLE001 - mDNS 查询失败不阻断后续
            log.debug("mdns services failed: %r", exc)

    new_port = mdns_port_for(host, mdns)
    if new_port and tcp_open(host, new_port, timeout=timeout):
        return Located(
            status=STATUS_MOVED,
            address=f"{host}:{new_port}",
            ip=host,
            port=new_port,
            mac=normalize_mac(mac),
            detail=f"IP 没变，无线调试端口换成了 {new_port}",
        )

    wanted_mac = normalize_mac(mac)
    if wanted_mac:
        table: dict[str, str] = {}
        if arp is not None:
            try:
                table = dict(arp() or {})
            except Exception as exc:  # noqa: BLE001
                log.debug("arp lookup failed: %r", exc)
        moved_ip = find_ip_by_mac(wanted_mac, table)
        if moved_ip and moved_ip != host:
            moved_port = mdns_port_for(moved_ip, mdns)
            if moved_port and tcp_open(moved_ip, moved_port, timeout=timeout):
                return Located(
                    status=STATUS_MOVED,
                    address=f"{moved_ip}:{moved_port}",
                    ip=moved_ip,
                    port=moved_port,
                    mac=wanted_mac,
                    detail=f"IP 从 {host} 换成了 {moved_ip}，端口 {moved_port}",
                )
            if tcp_open(moved_ip, 5555, timeout=timeout):
                return Located(
                    status=STATUS_MOVED,
                    address=f"{moved_ip}:5555",
                    ip=moved_ip,
                    port=5555,
                    mac=wanted_mac,
                    detail=f"IP 从 {host} 换成了 {moved_ip}（端口 5555）",
                )
            return Located(
                status=STATUS_NO_PORT,
                ip=moved_ip,
                port=0,
                mac=wanted_mac,
                detail=f"在 {moved_ip} 找到这台平板，但无线调试端口未知",
            )

    return Located(
        status=STATUS_OFFLINE,
        ip=host,
        port=port,
        mac=wanted_mac,
        detail="在局域网里找不到这台平板",
    )
