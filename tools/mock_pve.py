#!/usr/bin/env python3
"""A tiny in-memory imitation of the Proxmox VE API, for developing and
demoing ProxmoxPanel without a real cluster.

    python3 tools/mock_pve.py --port 8006

It serves HTTPS with a throwaway self-signed certificate (generated with
`openssl` on first start) and accepts:
  - password login:  root@pam / mockpass   (TOTP code 123456 for totp@pam)
  - API token:       root@pam!panel = mocktoken

State (guests, tasks, snapshots, firewall rules, backups...) lives in memory
and is lost on exit. Write operations return UPIDs whose tasks finish after
~1.5 seconds, so the app's task waiting / job log behave realistically.
"""

import argparse
import json
import os
import random
import re
import ssl
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, unquote, urlparse

LOCK = threading.Lock()
NOW = lambda: int(time.time())  # noqa: E731

NODES = {
    "pve1": {"maxcpu": 16, "maxmem": 64 * 2**30, "cpu": 0.12, "mem": 18 * 2**30},
    "pve2": {"maxcpu": 32, "maxmem": 128 * 2**30, "cpu": 0.31, "mem": 70 * 2**30},
}

STORAGES = {
    "local": {"type": "dir", "content": "iso,vztmpl,backup,snippets,import", "shared": 0, "total": 100 * 2**30, "used": 22 * 2**30},
    "local-lvm": {"type": "lvmthin", "content": "images,rootdir", "shared": 0, "total": 900 * 2**30, "used": 310 * 2**30},
    "ceph": {"type": "rbd", "content": "images,rootdir", "shared": 1, "total": 4000 * 2**30, "used": 1200 * 2**30},
}

CONTENT = {
    "local": [
        {"volid": "local:iso/debian-12.7.0-amd64-netinst.iso", "content": "iso", "format": "iso", "size": 661651456},
        {"volid": "local:iso/ubuntu-24.04.1-live-server-amd64.iso", "content": "iso", "format": "iso", "size": 2773874688},
        {"volid": "local:vztmpl/debian-12-standard_12.7-1_amd64.tar.zst", "content": "vztmpl", "format": "tzst", "size": 126473433},
        {"volid": "local:vztmpl/ubuntu-24.04-standard_24.04-2_amd64.tar.zst", "content": "vztmpl", "format": "tzst", "size": 141203651},
    ],
}

GUESTS = {}
TASKS = {}
SNAPSHOTS = {}
FIREWALL = {}
BACKUP_JOBS = []
HA = []
POOLS = [{"poolid": "customer-acme", "comment": "ACME Corp"}]
USERS = [
    {"userid": "root@pam", "enable": 1, "expire": 0, "email": "admin@example.com", "comment": "Built-in"},
    {"userid": "acme@pve", "enable": 1, "expire": 0, "email": "ops@acme.example", "comment": "tenant"},
]
ACL = [{"path": "/pool/customer-acme", "type": "user", "ugid": "acme@pve", "roleid": "PVEVMUser", "propagate": 1}]


def add_guest(kind, vmid, node, name, status="stopped", template=0, cores=2, mem_mb=2048, disk_gb=20, pool="", tags=""):
    cfg = {"cores": cores, "memory": mem_mb, "onboot": 1, "digest": "x" * 40}
    if kind == "qemu":
        cfg.update({
            "name": name, "sockets": 1, "ostype": "l26", "scsihw": "virtio-scsi-single", "agent": "1",
            "scsi0": f"local-lvm:vm-{vmid}-disk-0,iothread=1,size={disk_gb}G",
            "ide2": f"local-lvm:vm-{vmid}-cloudinit,media=cdrom",
            "boot": "order=scsi0;net0",
            "net0": f"virtio=BC:24:11:{vmid % 256:02X}:{random.randint(0, 255):02X}:01,bridge=vmbr0,firewall=1",
            "ipconfig0": "ip=dhcp", "ciuser": "debian",
        })
    else:
        cfg.update({
            "hostname": name, "swap": 512, "ostype": "debian", "unprivileged": 1, "features": "nesting=1",
            "rootfs": f"local-lvm:vm-{vmid}-disk-0,size={disk_gb}G",
            "net0": f"name=eth0,bridge=vmbr0,hwaddr=BC:24:11:{vmid % 256:02X}:00:02,ip=dhcp,type=veth,firewall=1",
        })
    if template:
        cfg["template"] = 1
    if tags:
        cfg["tags"] = tags
    GUESTS[vmid] = {"kind": kind, "node": node, "status": status, "template": template, "pool": pool,
                    "cfg": cfg, "started": NOW() - random.randint(600, 900000)}
    SNAPSHOTS.setdefault(vmid, [])
    FIREWALL.setdefault(vmid, {"options": {"enable": 0, "policy_in": "DROP", "policy_out": "ACCEPT"}, "rules": []})


add_guest("qemu", 100, "pve1", "web01", "running", tags="web;prod", pool="customer-acme")
add_guest("qemu", 101, "pve1", "db01", "running", cores=4, mem_mb=8192, disk_gb=80, tags="db;prod")
add_guest("lxc", 102, "pve2", "dns01", "running", cores=1, mem_mb=512, disk_gb=8, tags="infra")
add_guest("lxc", 103, "pve2", "staging", "stopped", tags="test")
add_guest("qemu", 9000, "pve1", "debian-12-cloud", template=1, disk_gb=3)
add_guest("qemu", 9001, "pve2", "ubuntu-24.04-cloud", template=1, disk_gb=4)
SNAPSHOTS[100] = [{"name": "before_upgrade", "description": "pre apt upgrade", "snaptime": NOW() - 86400 * 3}]
FIREWALL[100]["rules"] = [
    {"pos": 0, "type": "in", "action": "ACCEPT", "enable": 1, "macro": "SSH", "comment": "admin ssh"},
    {"pos": 1, "type": "in", "action": "ACCEPT", "enable": 1, "proto": "tcp", "dport": "80,443", "comment": "web"},
]
CONTENT["local"].append({"volid": "local:backup/vzdump-qemu-100-2026_10_01-02_00_01.vma.zst", "content": "backup",
                         "format": "vma.zst", "size": 3 * 2**30, "vmid": 100, "ctime": NOW() - 86400 * 2,
                         "notes": "web01 nightly"})
BACKUP_JOBS.append({"id": "backup-6f1c1d2a-01", "schedule": "02:00", "storage": "local", "vmid": "100,101",
                    "mode": "snapshot", "compress": "zstd", "enabled": 1, "prune-backups": "keep-last=3"})


def new_task(node, ttype, vid, ok=True, duration=1.5, log=None):
    upid = f"UPID:{node}:{random.randint(1, 2**31):08X}:{random.randint(1, 2**31):08X}:{NOW():08X}:{ttype}:{vid}:root@pam:"
    TASKS[upid] = {"node": node, "type": ttype, "id": str(vid), "user": "root@pam", "starttime": NOW(),
                   "done_at": time.time() + duration, "ok": ok,
                   "log": log or [f"starting {ttype} {vid}", "working...", "TASK OK" if ok else "TASK ERROR: simulated failure"]}
    return upid


def task_view(upid):
    t = TASKS[upid]
    done = time.time() >= t["done_at"]
    v = {"upid": upid, "node": t["node"], "type": t["type"], "id": t["id"], "user": t["user"], "starttime": t["starttime"],
         "status": "stopped" if done else "running"}
    if done:
        v["exitstatus"] = "OK" if t["ok"] else "simulated failure"
        v["endtime"] = int(t["done_at"])
    return v


def task_list_view(upid):
    """Task lists report the exit status in `status` once a task has ended."""
    v = task_view(upid)
    if v["status"] == "stopped":
        v["status"] = v.pop("exitstatus")
    return v


def resources():
    out = []
    for n, d in NODES.items():
        out.append({"id": f"node/{n}", "type": "node", "node": n, "status": "online", "cpu": d["cpu"] + random.uniform(-0.03, 0.03),
                    "maxcpu": d["maxcpu"], "mem": d["mem"], "maxmem": d["maxmem"], "disk": 9 * 2**30, "maxdisk": 98 * 2**30,
                    "uptime": 1234567})
        for s, sd in STORAGES.items():
            out.append({"id": f"storage/{n}/{s}", "type": "storage", "node": n, "storage": s, "status": "available",
                        "plugintype": sd["type"], "content": sd["content"], "shared": sd["shared"], "disk": sd["used"], "maxdisk": sd["total"]})
    for vmid, g in GUESTS.items():
        c = g["cfg"]
        running = g["status"] == "running"
        maxmem = int(c.get("memory", 512)) * 2**20
        size = re.search(r"size=(\d+)G", c.get("scsi0", c.get("rootfs", "size=8G")))
        out.append({"id": f"{g['kind']}/{vmid}", "type": g["kind"], "vmid": vmid, "node": g["node"],
                    "name": c.get("name", c.get("hostname", "")), "status": g["status"], "template": g["template"],
                    "cpu": random.uniform(0.01, 0.4) if running else 0, "maxcpu": int(c.get("cores", 1)),
                    "mem": int(maxmem * random.uniform(0.3, 0.7)) if running else 0, "maxmem": maxmem,
                    "disk": 0, "maxdisk": int(size.group(1)) * 2**30 if size else 0,
                    "uptime": NOW() - g["started"] if running else 0, "tags": c.get("tags", ""), "pool": g["pool"],
                    "lock": c.get("lock", ""), "hastate": next((h["state"] for h in HA if h["sid"].endswith(f":{vmid}")), "")})
    for p in POOLS:
        out.append({"id": f"pool/{p['poolid']}", "type": "pool", "pool": p["poolid"]})
    return out


def rrd(points=70, step=60, node=False):
    now = NOW()
    out = []
    for i in range(points):
        t = now - (points - i) * step
        e = {"time": t, "cpu": abs(0.2 + 0.15 * random.random()), "netin": random.uniform(5e3, 9e5), "netout": random.uniform(2e3, 4e5)}
        if node:
            e.update({"memused": 20 * 2**30 + random.uniform(0, 2**30), "memtotal": 64 * 2**30, "loadavg": random.uniform(0.2, 3)})
        else:
            e.update({"mem": 1.2 * 2**30 + random.uniform(0, 2**28), "maxmem": 2 * 2**30,
                      "diskread": random.uniform(0, 4e6), "diskwrite": random.uniform(0, 2e6)})
        out.append(e)
    return out


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        if os.environ.get("MOCK_VERBOSE"):
            super().log_message(fmt, *args)

    def reply(self, data, status=200, reason=None):
        body = json.dumps({"data": data} if status < 400 else {"data": None, "message": reason}).encode()
        self.send_response(status, reason)
        self.send_header("Content-Type", "application/json;charset=UTF-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def params(self):
        q = {k: v[-1] for k, v in parse_qs(urlparse(self.path).query).items()}
        n = int(self.headers.get("Content-Length") or 0)
        if n:
            raw = self.rfile.read(n)
            ctype = self.headers.get("Content-Type", "")
            if "multipart" in ctype:
                q["_upload_bytes"] = str(len(raw))
            else:
                for k, v in parse_qs(raw.decode(errors="replace"), keep_blank_values=True).items():
                    q[k] = v[-1] if len(v) == 1 else v
        return q

    def authed(self):
        auth = self.headers.get("Authorization", "")
        cookie = self.headers.get("Cookie", "")
        if auth == "PVEAPIToken=root@pam!panel=mocktoken":
            return True
        if "PVEAuthCookie=PVE:root@pam:MOCK" in cookie or "PVEAuthCookie=PVE:totp@pam:MOCK" in cookie:
            if self.command != "GET" and self.headers.get("CSRFPreventionToken") != "MOCKCSRF":
                return False
            return True
        return False

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")

    def do_PUT(self):
        self.route("PUT")

    def do_DELETE(self):
        self.route("DELETE")

    def route(self, method):
        path = unquote(urlparse(self.path).path)
        if not path.startswith("/api2/json"):
            return self.reply(None, 404, "Not Found")
        p = path[len("/api2/json"):].rstrip("/")
        args = self.params()
        if p == "/access/ticket" and method == "POST":
            user, pw = args.get("username"), args.get("password", "")
            if user == "root@pam" and pw in ("mockpass", "PVE:root@pam:MOCK"):
                return self.reply({"ticket": "PVE:root@pam:MOCK", "CSRFPreventionToken": "MOCKCSRF", "username": user})
            if user == "totp@pam" and pw == "mockpass":
                return self.reply({"ticket": "PVE:totp@pam:!tfa!challenge", "NeedTFA": 1, "username": user})
            if user == "totp@pam" and pw == "totp:123456":
                return self.reply({"ticket": "PVE:totp@pam:MOCK", "CSRFPreventionToken": "MOCKCSRF", "username": user})
            if user == "totp@pam" and pw == "PVE:totp@pam:MOCK":
                return self.reply({"ticket": "PVE:totp@pam:MOCK", "CSRFPreventionToken": "MOCKCSRF", "username": user})
            return self.reply(None, 401, "authentication failure")
        if not self.authed():
            return self.reply(None, 401, "No ticket")
        with LOCK:
            try:
                res = self.dispatch(method, p, args)
            except KeyError as e:
                return self.reply(None, 500, f"Configuration file does not exist ({e})")
            except ValueError as e:
                return self.reply(None, 400, f"Parameter verification failed. {e}")
        if res is NotImplemented:
            return self.reply([] if method == "GET" else None)
        return self.reply(res)

    def dispatch(self, method, p, a):
        m = lambda rx: re.fullmatch(rx, p)  # noqa: E731
        if p == "/version":
            return {"version": "9.1.2", "release": "9.1", "repoid": "mock"}
        if p == "/cluster/status":
            return [{"type": "cluster", "name": "mock-cluster", "quorate": 1, "nodes": 2}] + [
                {"type": "node", "name": n, "online": 1, "ip": f"10.0.0.{i + 11}", "local": int(i == 0)} for i, n in enumerate(NODES)]
        if p == "/cluster/resources":
            r = resources()
            return [x for x in r if x["type"] in ("qemu", "lxc")] if a.get("type") == "vm" else r
        if p == "/cluster/nextid":
            if "vmid" in a:
                if int(a["vmid"]) in GUESTS:
                    raise ValueError(f"vmid: VM {a['vmid']} already exists")
                return a["vmid"]
            return str(max([100] + [v + 1 for v in GUESTS if v < 9000]))
        if p == "/cluster/tasks":
            return sorted((task_list_view(u) for u in TASKS), key=lambda t: -t["starttime"])
        if p == "/nodes":
            return [{"node": n, "status": "online", "cpu": d["cpu"], "maxcpu": d["maxcpu"], "mem": d["mem"], "maxmem": d["maxmem"]} for n, d in NODES.items()]
        if p == "/storage":
            return [{"storage": s, "type": d["type"], "content": d["content"], "shared": d["shared"]} for s, d in STORAGES.items()]
        if p == "/access/permissions":
            return {"/": {k: 1 for k in ["Sys.Audit", "Sys.Modify", "VM.Allocate", "VM.Audit", "VM.PowerMgmt", "VM.Console", "VM.Config.Disk",
                                         "VM.Snapshot", "VM.Backup", "Datastore.AllocateSpace", "Pool.Allocate", "Permissions.Modify"]}}
        if p == "/pools":
            if method == "POST":
                POOLS.append({"poolid": a["poolid"], "comment": a.get("comment", "")})
                return None
            return POOLS
        if m(r"/pools/[^/]+") and method == "PUT":
            pool = p.split("/")[2]
            for v in str(a.get("vms", "")).split(","):
                if v.strip().isdigit() and int(v) in GUESTS:
                    GUESTS[int(v)]["pool"] = "" if a.get("delete") == "1" else pool
            return None
        if p == "/access/users":
            if method == "POST":
                USERS.append({"userid": a["userid"], "enable": 1, "expire": 0, "email": a.get("email", ""), "comment": a.get("comment", "")})
                return None
            return USERS
        if p == "/access/acl":
            if method == "PUT":
                who = a.get("users") or a.get("groups") or a.get("tokens")
                if a.get("delete") == "1":
                    ACL[:] = [x for x in ACL if not (x["path"] == a["path"] and x["ugid"] == who)]
                else:
                    ACL.append({"path": a["path"], "type": "user", "ugid": who, "roleid": a["roles"], "propagate": 1})
                return None
            return ACL
        if p == "/access/roles":
            return [{"roleid": r} for r in ["Administrator", "PVEAdmin", "PVEVMAdmin", "PVEVMUser", "PVEAuditor", "NoAccess"]]
        if m(r"/access/users/[^/]+/token(/[^/]+)?"):
            if method == "POST":
                return {"full-tokenid": f"{p.split('/')[3]}!{p.split('/')[5]}", "value": "11111111-2222-3333-4444-555555555555"}
            return [{"tokenid": "panel", "privsep": 0}]
        if p == "/cluster/backup":
            if method == "POST":
                BACKUP_JOBS.append({"id": f"backup-{random.randint(0, 2**32):08x}", **{k: v for k, v in a.items() if not k.startswith('_')}})
                return None
            return BACKUP_JOBS
        if m(r"/cluster/backup/[^/]+"):
            jid = p.split("/")[3]
            if method == "DELETE":
                BACKUP_JOBS[:] = [j for j in BACKUP_JOBS if j["id"] != jid]
            return None
        if p == "/cluster/ha/resources":
            if method == "POST":
                HA.append({"sid": a["sid"], "state": a.get("state", "started"), "max_restart": 1, "max_relocate": 1})
                return None
            return HA
        if m(r"/cluster/ha/resources/[^/]+"):
            sid = p.split("/")[4]
            if method == "DELETE":
                HA[:] = [h for h in HA if h["sid"] != sid]
            elif method == "PUT":
                for h in HA:
                    if h["sid"] == sid:
                        h["state"] = a.get("state", h["state"])
            return None
        if p == "/cluster/ha/status/current":
            return [{"type": "quorum", "status": "OK"}, {"type": "master", "status": "pve1 (active)"}]
        if p == "/cluster/firewall/macros":
            return [{"macro": x} for x in ["SSH", "HTTP", "HTTPS", "DNS", "Ping", "MySQL", "PostgreSQL", "RDP"]]

        mm = m(r"/nodes/([^/]+)(/.*)?")
        if not mm:
            return NotImplemented
        node, rest = mm.group(1), mm.group(2) or ""
        if rest == "/status":
            if method == "POST":
                return new_task(node, "srvreload", "")
            d = NODES[node]
            return {"cpu": d["cpu"], "memory": {"used": d["mem"], "total": d["maxmem"]}, "swap": {"used": 0, "total": 8 * 2**30},
                    "rootfs": {"used": 9 * 2**30, "total": 98 * 2**30}, "loadavg": ["0.42", "0.51", "0.48"],
                    "cpuinfo": {"model": "AMD EPYC 7443P", "sockets": 1, "cpus": d["maxcpu"]}, "kversion": "Linux 6.14.8-2-pve",
                    "pveversion": "pve-manager/9.1.2", "uptime": 1234567}
        if rest.startswith("/rrddata"):
            return rrd(node=True)
        if rest == "/network":
            return [{"iface": "vmbr0", "type": "bridge", "active": 1, "cidr": "10.0.0.11/24", "gateway": "10.0.0.1", "bridge_ports": "eno1"},
                    {"iface": "vmbr1", "type": "bridge", "active": 1, "bridge_ports": "eno2"},
                    {"iface": "eno1", "type": "eth", "active": 1}]
        if rest == "/apt/update":
            return [{"Package": "pve-manager"}, {"Package": "proxmox-kernel"}]
        if rest == "/aplinfo":
            if method == "POST":
                CONTENT["local"].append({"volid": f"local:vztmpl/{a['template']}", "content": "vztmpl", "format": "tzst", "size": 120 * 2**20})
                return new_task(node, "download", a["template"])
            return [{"section": "system", "template": "alpine-3.20-default_20240908_amd64.tar.xz", "headline": "Alpine Linux 3.20"},
                    {"section": "system", "template": "rockylinux-9-default_20240912_amd64.tar.xz", "headline": "Rocky Linux 9"}]
        if rest == "/storage":
            out = []
            for s, d in STORAGES.items():
                if a.get("content") and a["content"] not in d["content"].split(","):
                    continue
                out.append({"storage": s, "type": d["type"], "content": d["content"], "active": 1, "enabled": 1, "shared": d["shared"],
                            "total": d["total"], "used": d["used"], "avail": d["total"] - d["used"]})
            return out
        sm = re.fullmatch(r"/storage/([^/]+)/(content|upload|download-url)(/.*)?", rest)
        if sm:
            st, what, vol = sm.group(1), sm.group(2), (sm.group(3) or "")[1:]
            items = CONTENT.setdefault(st, [])
            if what == "upload":
                return new_task(node, "imgcopy", "")
            if what == "download-url":
                # URLs containing "404" fail permanently, like a missing image upstream.
                if "404" in a.get("url", ""):
                    return new_task(node, "download", a["filename"], ok=False, duration=2,
                                    log=[f"downloading {a.get('url')} to {a['filename']}",
                                         "download failed: 404 Not Found", "TASK ERROR: download failed: 404 Not Found"])
                fmt = a["filename"].rsplit(".", 1)[-1]
                items.append({"volid": f"{st}:{a['content']}/{a['filename']}", "content": a["content"], "format": fmt, "size": 600 * 2**20})
                return new_task(node, "download", a["filename"], duration=3,
                                log=[f"downloading {a.get('url')} to {a['filename']}", "  10% ...", "  60% ...", "100% done", "TASK OK"])
            if vol and method == "DELETE":
                CONTENT[st] = [i for i in items if i["volid"] != vol]
                return new_task(node, "imgdel", "")
            if vol and method == "PUT":
                for i in items:
                    if i["volid"] == vol:
                        i["protected"] = int(a.get("protected", 0))
                return None
            res = [dict(i, ctime=i.get("ctime", NOW() - 86400 * 10)) for i in items]
            if a.get("content"):
                res = [i for i in res if i["content"] == a["content"]]
            if a.get("vmid"):
                res = [i for i in res if str(i.get("vmid", "")) == a["vmid"]]
            return res
        if rest == "/vzdump" and method == "POST":
            for v in str(a["vmid"]).split(","):
                CONTENT.setdefault(a["storage"], []).append({
                    "volid": f"{a['storage']}:backup/vzdump-{GUESTS[int(v)]['kind']}-{v}-{time.strftime('%Y_%m_%d-%H_%M_%S')}.vma.zst",
                    "content": "backup", "format": "vma.zst", "size": 2 * 2**30, "vmid": int(v), "ctime": NOW(), "notes": a.get("notes-template", "")})
            return new_task(node, "vzdump", a["vmid"])
        tm = re.fullmatch(r"/tasks(/(UPID:[^/]+))?(/status|/log)?", rest)
        if tm:
            upid = tm.group(2)
            if not upid:
                vmid = a.get("vmid")
                return [task_list_view(u) for u, t in TASKS.items() if t["node"] == node and (not vmid or t["id"] == vmid)]
            if method == "DELETE":
                TASKS[upid]["done_at"] = time.time()
                TASKS[upid]["ok"] = False
                return None
            if tm.group(3) == "/status":
                return task_view(upid)
            return [{"n": i + 1, "t": line} for i, line in enumerate(TASKS[upid]["log"])]
        if rest in ("/qemu", "/lxc") and method == "POST":
            kind = rest[1:]
            vmid = int(a["vmid"])
            if vmid in GUESTS and a.get("force") != "1":
                raise ValueError(f"vmid: VM {vmid} already exists")
            if a.get("archive") or a.get("restore") == "1":
                return new_task(node, "qmrestore" if kind == "qemu" else "vzrestore", vmid)
            name = a.get("name") or a.get("hostname") or f"guest{vmid}"
            add_guest(kind, vmid, node, name, cores=int(a.get("cores", 1)), mem_mb=int(a.get("memory", 512)), pool=a.get("pool", ""))
            g = GUESTS[vmid]["cfg"]
            for k, v in a.items():
                if not k.startswith("_") and k not in ("vmid", "pool", "password", "ssh-public-keys", "start", "storage"):
                    g[k] = v
            # Disks given as `storage:size` / `import-from=` become allocated volumes.
            for k, v in list(g.items()):
                if re.fullmatch(r"(scsi|virtio|sata|ide|efidisk|tpmstate)\d+|rootfs|mp\d+", k) and isinstance(v, str) and "media=cdrom" not in v:
                    head, _, opts = v.partition(",")
                    mt = re.fullmatch(r"([^:]+):(\d+)", head)
                    if mt:
                        size = "3" if "import-from=" in opts else mt.group(2)
                        opts = ",".join(o for o in opts.split(",") if o and not o.startswith("import-from=") and not o.startswith("size="))
                        g[k] = f"{mt.group(1)}:vm-{vmid}-disk-{k},size={size}G" + (f",{opts}" if opts else "")
            return new_task(node, "qmcreate" if kind == "qemu" else "vzcreate", vmid, duration=2.5)
        gm = re.fullmatch(r"/(qemu|lxc)/(\d+)(/.*)?", rest)
        if not gm:
            return NotImplemented
        kind, vmid, sub = gm.group(1), int(gm.group(2)), gm.group(3) or ""
        g = GUESTS[vmid]
        cfg = g["cfg"]
        if sub == "" and method == "DELETE":
            del GUESTS[vmid]
            FIREWALL.pop(vmid, None)  # like PVE, destroying a guest deletes its firewall config
            return new_task(node, "qmdestroy" if kind == "qemu" else "vzdestroy", vmid)
        if sub == "/config":
            if method == "GET":
                return cfg
            for k in str(a.get("delete", "")).split(","):
                cfg.pop(k.strip(), None)
            for k, v in a.items():
                if k not in ("delete", "digest", "skiplock") and not k.startswith("_"):
                    cfg[k] = v
            return new_task(node, "qmconfig", vmid, duration=0.5) if kind == "qemu" and method == "POST" else None
        if sub == "/pending":
            return [{"key": k, "value": v} for k, v in cfg.items()]
        if sub == "/status/current":
            running = g["status"] == "running"
            mx = int(cfg.get("memory", 512)) * 2**20
            return {"status": g["status"], "cpu": random.uniform(0.01, 0.3) if running else 0, "cpus": int(cfg.get("cores", 1)),
                    "mem": int(mx * 0.45) if running else 0, "maxmem": mx, "disk": 2 * 2**30, "maxdisk": 20 * 2**30,
                    "uptime": NOW() - g["started"] if running else 0, "qmpstatus": g["status"], "agent": 1 if kind == "qemu" else 0,
                    "swap": 0, "maxswap": 512 * 2**20}
        sm2 = re.fullmatch(r"/status/(start|stop|shutdown|reboot|reset|suspend|resume)", sub)
        if sm2:
            act = sm2.group(1)
            g["status"] = {"start": "running", "stop": "stopped", "shutdown": "stopped", "reboot": "running",
                           "reset": "running", "suspend": "paused", "resume": "running"}[act]
            if act in ("start", "reboot", "reset"):
                g["started"] = NOW()
            prefix = "qm" if kind == "qemu" else "vz"
            return new_task(node, prefix + act, vmid)
        if sub.startswith("/rrddata"):
            tf = a.get("timeframe", "hour")
            return rrd(step={"hour": 60, "day": 1200, "week": 8640, "month": 37000, "year": 450000}.get(tf, 60))
        if sub == "/snapshot":
            if method == "POST":
                SNAPSHOTS[vmid].append({"name": a["snapname"], "description": a.get("description", ""), "snaptime": NOW(), "vmstate": int(a.get("vmstate", 0))})
                return new_task(node, "qmsnapshot", vmid)
            return SNAPSHOTS[vmid] + [{"name": "current", "description": "You are here!"}]
        sn = re.fullmatch(r"/snapshot/([^/]+)(/rollback|/config)?", sub)
        if sn:
            if method == "DELETE":
                SNAPSHOTS[vmid] = [s for s in SNAPSHOTS[vmid] if s["name"] != sn.group(1)]
                return new_task(node, "qmdelsnapshot", vmid)
            return new_task(node, "qmrollback", vmid)
        if sub.startswith("/feature"):
            return {"hasFeature": 1, "nodes": list(NODES)}
        if sub == "/clone":
            newid = int(a["newid"])
            target = a.get("target", node)
            add_guest(kind, newid, target, a.get("name") or a.get("hostname") or f"clone{newid}", cores=int(cfg.get("cores", 1)),
                      mem_mb=int(cfg.get("memory", 512)), pool=a.get("pool", ""))
            return new_task(node, "qmclone", vmid, duration=3)
        if sub == "/migrate":
            g["node"] = a["target"]
            return new_task(node, "qmigrate", vmid, duration=3)
        if sub == "/template":
            g["template"] = 1
            cfg["template"] = 1
            return new_task(node, "qmtemplate", vmid)
        if sub == "/resize":
            for k in ("scsi0", "rootfs", a.get("disk")):
                if k and k in cfg and k == a.get("disk"):
                    cfg[k] = re.sub(r"size=\d+G", f"size={a['size'].rstrip('G').lstrip('+')}G", cfg[k])
            return new_task(node, "resize", vmid)
        if sub in ("/move_disk", "/move_volume"):
            return new_task(node, "qmmove", vmid)
        if sub == "/cloudinit":
            return None
        if sub == "/interfaces":
            return [{"name": "lo", "inet": "127.0.0.1/8"}, {"name": "eth0", "inet": f"10.0.0.{vmid % 200}/24", "inet6": "2001:db8::%x/64" % vmid}]
        if sub.startswith("/agent/"):
            cmd = sub[len("/agent/"):]
            if g["status"] != "running":
                raise KeyError("VM is not running")
            if cmd == "ping":
                return None
            if cmd == "get-osinfo":
                return {"result": {"pretty-name": "Debian GNU/Linux 12 (bookworm)", "kernel-release": "6.1.0-26-amd64", "machine": "x86_64"}}
            if cmd == "get-fsinfo":
                return {"result": [{"mountpoint": "/", "type": "ext4", "total-bytes": 20 * 2**30, "used-bytes": 6 * 2**30}]}
            if cmd == "network-get-interfaces":
                return {"result": [{"name": "lo", "ip-addresses": [{"ip-address": "127.0.0.1", "prefix": 8}]},
                                   {"name": "eth0", "ip-addresses": [{"ip-address": f"10.0.0.{vmid % 200}", "prefix": 24},
                                                                     {"ip-address": "fe80::1", "prefix": 64}]}]}
            if cmd == "exec":
                return {"pid": 4242}
            if cmd.startswith("exec-status"):
                return {"exited": 1, "exitcode": 0, "out-data": "hello from the mock guest agent\n"}
            if cmd == "set-user-password":
                return None
            return {"result": {}}
        if sub == "/firewall/aliases":
            aliases = FIREWALL[vmid].setdefault("aliases", [])
            if method == "POST":
                aliases.append({k: v for k, v in a.items() if not k.startswith("_")})
                return None
            return aliases
        ips = re.fullmatch(r"/firewall/ipset(/([^/]+)(/(.+))?)?", sub)
        if ips:
            sets = FIREWALL[vmid].setdefault("ipsets", {})
            name, cidr = ips.group(2), ips.group(4)
            if not name:
                if method == "POST":
                    sets.setdefault(a["name"], {"comment": a.get("comment", ""), "entries": []})
                    return None
                return [{"name": n, "comment": d["comment"]} for n, d in sets.items()]
            if name not in sets:
                raise KeyError(f"no such IPSet '{name}'")
            entries = sets[name]["entries"]
            if cidr:
                if method == "DELETE":
                    sets[name]["entries"] = [e for e in entries if e["cidr"] != cidr]
                return None
            if method == "DELETE":
                if entries:
                    raise ValueError("IPSet is not empty")
                del sets[name]
                return None
            if method == "POST":
                if any(e["cidr"] == a["cidr"] for e in entries):
                    raise ValueError(f"entry '{a['cidr']}' already exists")
                entries.append({"cidr": a["cidr"], "comment": a.get("comment", "")})
                return None
            return entries
        fw = re.fullmatch(r"/firewall/(options|rules|log)(/(\d+))?", sub)
        if fw:
            f = FIREWALL[vmid]
            if fw.group(1) == "options":
                if method == "PUT":
                    f["options"].update({k: v for k, v in a.items() if not k.startswith("_")})
                return f["options"]
            if fw.group(1) == "log":
                return [{"n": 1, "t": f"{vmid} 6 tap{vmid}i0-IN 01/Oct/2026:10:00:00 +0000 ACCEPT: IN=fwbr SRC=203.0.113.9 DST=10.0.0.5 PROTO=TCP DPT=22"}]
            rules = f["rules"]
            if fw.group(3) is not None:
                pos = int(fw.group(3))
                if method == "DELETE":
                    del rules[pos]
                elif method == "PUT":
                    if "moveto" in a:
                        r = rules.pop(pos)
                        to = int(a["moveto"])
                        rules.insert(to if to < pos else to - 1, r)
                    else:
                        rules[pos].update({k: v for k, v in a.items() if not k.startswith("_")})
                for i, r in enumerate(rules):
                    r["pos"] = i
                return None
            if method == "POST":
                rules.insert(0, {k: v for k, v in a.items() if not k.startswith("_")})
                for i, r in enumerate(rules):
                    r["pos"] = i
                return None
            return rules
        if sub == "/spiceproxy":
            return {"type": "spice", "host": "pvespiceproxy:mock", "password": "x", "proxy": "http://localhost:3128"}
        return NotImplemented


def ensure_cert(directory):
    crt, key = os.path.join(directory, "mock.crt"), os.path.join(directory, "mock.key")
    if not os.path.exists(crt):
        subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", crt,
                        "-days", "3650", "-subj", "/CN=mock-pve"], check=True, capture_output=True)
    return crt, key


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8006)
    ap.add_argument("--bind", default="127.0.0.1")
    ap.add_argument("--certdir", default=os.path.join(tempfile.gettempdir(), "mock-pve"))
    args = ap.parse_args()
    os.makedirs(args.certdir, exist_ok=True)
    crt, key = ensure_cert(args.certdir)
    fp = subprocess.run(["openssl", "x509", "-in", crt, "-noout", "-fingerprint", "-sha256"], capture_output=True, text=True).stdout.strip()
    srv = ThreadingHTTPServer((args.bind, args.port), Handler)
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    ctx.load_cert_chain(crt, key)
    srv.socket = ctx.wrap_socket(srv.socket, server_side=True)
    print(f"mock Proxmox VE API on https://{args.bind}:{args.port}  ({fp})", flush=True)
    print("login: root@pam / mockpass   token: root@pam!panel = mocktoken", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
