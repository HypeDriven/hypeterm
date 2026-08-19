#!/usr/bin/env python3
"""Drive the app on a connected device by element text rather than coordinates.

Hard-coded taps break the moment a layout shifts — inset padding, a font-size change, a
different screen. This dumps the accessibility hierarchy and taps the centre of the
node whose text or content description matches, which is stable across all of those.

Only a development helper for the manual on-device walkthrough in
docs/android-build.md; nothing ships with it.

  python3 tools/device_ui.py tap "Generate a device key"
  python3 tools/device_ui.py type "Identity ID" "2c-_fjX…"
  python3 tools/device_ui.py text "Public key:"
  python3 tools/device_ui.py dump
"""

import os
import re
import subprocess
import sys
import time

# On WSL2 this wants the Windows adb.exe, which talks to the device the host sees;
# the Linux adb in the distro does not. Set $ADB to it, e.g.
#   /mnt/c/Users/<you>/AppData/Local/Android/Sdk/platform-tools/adb.exe
ADB = os.environ.get("ADB", "adb")


def adb(*args, binary=False):
    result = subprocess.run([ADB, *args], capture_output=True)
    if result.returncode != 0 and not binary:
        sys.stderr.write(result.stderr.decode(errors="replace"))
    return result.stdout if binary else result.stdout.decode(errors="replace")


def hierarchy():
    adb("shell", "uiautomator", "dump", "/sdcard/ui.xml")
    return adb("exec-out", "cat", "/sdcard/ui.xml", binary=True).decode(
        "utf-8", errors="replace")


def nodes(xml):
    for match in re.finditer(r"<node\b[^>]*>", xml):
        node = match.group(0)
        text = re.search(r'text="([^"]*)"', node)
        description = re.search(r'content-desc="([^"]*)"', node)
        bounds = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', node)
        if not bounds:
            continue
        left, top, right, bottom = (int(v) for v in bounds.groups())
        yield {
            "text": (text.group(1) if text else "").replace("&#10;", "\n"),
            "desc": (description.group(1) if description else ""),
            "centre": ((left + right) // 2, (top + bottom) // 2),
        }


def find(needle, xml=None):
    xml = xml or hierarchy()
    for node in nodes(xml):
        if needle in node["text"] or needle in node["desc"]:
            return node
    return None


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    command = sys.argv[1]

    if command == "dump":
        for node in nodes(hierarchy()):
            if node["text"] or node["desc"]:
                print(f'{node["centre"]}  {node["text"] or node["desc"]!r}')
        return 0

    if command == "text":
        node = find(sys.argv[2])
        if node is None:
            sys.stderr.write(f"not found: {sys.argv[2]}\n")
            return 1
        print(node["text"])
        return 0

    if command == "tap":
        node = find(sys.argv[2])
        if node is None:
            sys.stderr.write(f"not found: {sys.argv[2]}\n")
            return 1
        x, y = node["centre"]
        adb("shell", "input", "tap", str(x), str(y))
        return 0

    if command == "type":
        node = find(sys.argv[2])
        if node is None:
            sys.stderr.write(f"not found: {sys.argv[2]}\n")
            return 1
        x, y = node["centre"]
        adb("shell", "input", "tap", str(x), str(y))
        time.sleep(0.5)
        adb("shell", "input", "text", sys.argv[3])
        time.sleep(0.3)
        adb("shell", "input", "keyevent", "111")  # close the IME
        return 0

    sys.stderr.write(f"unknown command: {command}\n")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
