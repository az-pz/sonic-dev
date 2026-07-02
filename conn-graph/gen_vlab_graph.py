#!/usr/bin/env python3
"""Generate a lab connection graph (devices + links CSV) for the KVM DUT vlab-01.

Source of truth: the real CONFIG_DB DEVICE_NEIGHBOR + PORT tables dumped from the
running vlab-01 DUT. Output goes to dev/conn-graph/ and is injected into the
docker-sonic-mgmt container's ansible/files/ at test time (never committed into
the sonic-mgmt repo).
"""
import csv
import os

DUT = "vlab-01"
DUT_IP = "10.250.0.101/24"
DUT_HWSKU = "Force10-S6000"

# port -> (peerdevice, peerport), taken verbatim from vlab-01 DEVICE_NEIGHBOR.
NEIGHBORS = {
    "Ethernet4":  ("Servers0",  "eth0"),
    "Ethernet8":  ("Servers1",  "eth0"),
    "Ethernet12": ("Servers2",  "eth0"),
    "Ethernet16": ("Servers3",  "eth0"),
    "Ethernet20": ("Servers4",  "eth0"),
    "Ethernet24": ("Servers5",  "eth0"),
    "Ethernet28": ("Servers6",  "eth0"),
    "Ethernet32": ("Servers7",  "eth0"),
    "Ethernet36": ("Servers8",  "eth0"),
    "Ethernet40": ("Servers9",  "eth0"),
    "Ethernet44": ("Servers10", "eth0"),
    "Ethernet48": ("Servers11", "eth0"),
    "Ethernet52": ("Servers12", "eth0"),
    "Ethernet56": ("Servers13", "eth0"),
    "Ethernet60": ("Servers14", "eth0"),
    "Ethernet64": ("Servers15", "eth0"),
    "Ethernet68": ("Servers16", "eth0"),
    "Ethernet72": ("Servers17", "eth0"),
    "Ethernet76": ("Servers18", "eth0"),
    "Ethernet80": ("Servers19", "eth0"),
    "Ethernet84": ("Servers20", "eth0"),
    "Ethernet88": ("Servers21", "eth0"),
    "Ethernet92": ("Servers22", "eth0"),
    "Ethernet96": ("Servers23", "eth0"),
    "Ethernet112": ("ARISTA01T1", "Ethernet1"),
    "Ethernet116": ("ARISTA02T1", "Ethernet1"),
    "Ethernet120": ("ARISTA03T1", "Ethernet1"),
    "Ethernet124": ("ARISTA04T1", "Ethernet1"),
}

SPEED = "40000"

out_dir = os.path.dirname(os.path.abspath(__file__))
os.makedirs(out_dir, exist_ok=True)

# ---- devices csv ----
devices = []
devices.append([DUT, DUT_IP, DUT_HWSKU, "DevSonic", "", "sonic", ""])

peers = {}
for port, (peer, peerport) in NEIGHBORS.items():
    peers.setdefault(peer, peerport)

srv_ip = 1
ar_ip = 1
for peer in sorted(peers):
    if peer.startswith("ARISTA"):
        devices.append([peer, "10.64.1.{}/24".format(ar_ip), "Arista-VM", "DevSonic", "", "eos", ""])
        ar_ip += 1
    else:
        devices.append([peer, "10.64.0.{}/24".format(srv_ip), "TestServ", "Server", "", "ubuntu", ""])
        srv_ip += 1

with open(os.path.join(out_dir, "sonic_vlab_devices.csv"), "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["Hostname", "ManagementIp", "HwSku", "Type", "Protocol", "Os", "AuthType"])
    w.writerows(devices)

# ---- links csv ----
def portnum(p):
    return int(p.replace("Ethernet", ""))

with open(os.path.join(out_dir, "sonic_vlab_links.csv"), "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["StartDevice", "StartPort", "EndDevice", "EndPort", "BandWidth", "VlanID", "VlanMode", "AutoNeg"])
    for port in sorted(NEIGHBORS, key=portnum):
        peer, peerport = NEIGHBORS[port]
        w.writerow([DUT, port, peer, peerport, SPEED, "", "", ""])

print("Wrote {} devices and {} links to {}".format(len(devices), len(NEIGHBORS), out_dir))
