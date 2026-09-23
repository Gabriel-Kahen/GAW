#!/usr/bin/env python3
"""Read-only Linux audio diagnostics; never opens PCM devices or records samples."""

import argparse
import json
from pathlib import Path
import statistics
import subprocess
import time


def command(*args):
    try:
        result = subprocess.run(args, capture_output=True, text=True, timeout=8, check=False)
        return result.stdout.strip() or result.stderr.strip()
    except (OSError, subprocess.TimeoutExpired) as error:
        return str(error)


def fields(path):
    try:
        return dict((key.strip(), value.strip())
                    for key, value in (line.split(":", 1)
                                       for line in path.read_text().splitlines() if ":" in line))
    except OSError:
        return {}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--match", default="gaw|Scarlett|hw_USB", help="Node name/description regex")
    parser.add_argument("--seconds", type=float, default=2, help="Hardware-delay sampling duration (0–30)")
    args = parser.parse_args()
    if not 0 <= args.seconds <= 30:
        parser.error("--seconds must be between 0 and 30")

    import re

    try:
        pattern = re.compile(args.match, re.IGNORECASE)
    except re.error as error:
        parser.error(str(error))
    print("Metadata (configured quantum, not necessarily the running driver quantum):")
    print(command("pw-metadata", "-n", "settings"))
    raw = command("pw-dump")
    try:
        objects = json.loads(raw)
    except (ValueError, TypeError):
        objects = []
        print("PipeWire graph unavailable:", raw)
    nodes = {
        obj["id"]: obj["info"]
        for obj in objects
        if obj.get("type", "").endswith(":Node") and obj.get("info")
    }
    selected = {
        node_id: info for node_id, info in nodes.items()
        if pattern.search(" ".join(str(info.get("props", {}).get(key, ""))
                                   for key in ("node.name", "node.description", "application.name")))
    }
    print("\nSelected graph nodes:")
    cards, pids = set(), set()
    for node_id, info in selected.items():
        props = info.get("props", {})
        print(node_id, info.get("state"), json.dumps({
            key: value for key, value in props.items()
            if key.startswith(("node.", "audio.", "api.alsa.", "application.process."))
        }, sort_keys=True))
        print("  Latency:", json.dumps(info.get("params", {}).get("Latency", [])))
        print("  ProcessLatency:", json.dumps(info.get("params", {}).get("ProcessLatency", [])))
        if "api.alsa.pcm.card" in props:
            cards.add(str(props["api.alsa.pcm.card"]))
        if "application.process.id" in props:
            pids.add(str(props["application.process.id"]))
    print("\nLinks touching selected nodes:")
    for obj in objects:
        if not obj.get("type", "").endswith(":Link"):
            continue
        info = obj.get("info", {})
        source, target = info.get("output-node-id"), info.get("input-node-id")
        if source in selected or target in selected:
            name = lambda node_id: nodes.get(node_id, {}).get("props", {}).get("node.name", node_id)
            print(name(source), "->", name(target), info.get("state"))

    paths = [path for card in sorted(cards)
             for path in sorted(Path(f"/proc/asound/card{card}").glob("pcm*/sub*/hw_params"))]
    print("\nALSA hardware parameters (ring capacity is not measured latency):")
    active = []
    for path in paths:
        params = fields(path)
        print(path, params or "closed/unavailable")
        if "rate" in params:
            active.append((path.parent / "status", int(params["rate"].strip().split()[0])))
    samples = {path: [] for path, _ in active}
    deadline = time.monotonic() + args.seconds
    while active and time.monotonic() < deadline:
        for path, _ in active:
            value = fields(path).get("delay")
            if value is not None:
                try:
                    samples[path].append(int(value))
                except ValueError:
                    pass
        time.sleep(0.01)
    print("\nInstantaneous hardware delay min/median/max; not full app round-trip latency:")
    for path, rate in active:
        values = samples[path]
        if values:
            frames = [min(values), statistics.median(values), max(values)]
            print(path, len(values), "samples; frames", frames,
                  "; ms", [round(value * 1000 / rate, 3) for value in frames])
    print("\nAudio thread scheduling:")
    pids.update(command("pgrep", "-x", "gaw-app").split())
    pids.update(command("pgrep", "-x", "pipewire").split())
    for pid in sorted(pid for pid in pids if pid.isdecimal()):
        print(command("ps", "-T", "-p", pid, "-o", "pid,tid,cls,rtprio,comm"))
    print("\nRunning graph quantum and error counters (second snapshot is initialized):")
    print(command("pw-top", "-b", "-n", "2"))


if __name__ == "__main__":
    main()
