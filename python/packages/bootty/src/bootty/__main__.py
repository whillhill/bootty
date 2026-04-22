from __future__ import annotations

import importlib
import platform
import stat
import subprocess
import sys
from pathlib import Path

ARCH_ALIASES = {
    "x86_64": "x64",
    "amd64": "x64",
    "arm64": "arm64",
    "aarch64": "arm64",
}

PLATFORM_MODULE_MAP = {
    "darwin-arm64": "bootty_bin_darwin_arm64",
    "darwin-x64": "bootty_bin_darwin_x64",
    "linux-x64": "bootty_bin_linux_x64_gnu",
    "windows-x64": "bootty_bin_win32_x64_msvc",
}


def current_platform_key() -> str:
    system = platform.system().lower()
    machine = ARCH_ALIASES.get(platform.machine().lower(), platform.machine().lower())
    return f"{system}-{machine}"


def resolve_binary(module_name: str) -> Path:
    module = importlib.import_module(module_name)
    binary_fn = getattr(module, "binary_path", None)
    if binary_fn is None:
        raise RuntimeError(f"Module is missing binary_path(): {module_name}")

    binary = Path(binary_fn())
    if not binary.exists():
        raise RuntimeError(f"Platform binary does not exist: {binary}")
    return binary


def main() -> int:
    key = current_platform_key()
    module_name = PLATFORM_MODULE_MAP.get(key)
    if module_name is None:
        print(f"[error] Unsupported platform: {platform.system()}/{platform.machine()}")
        return 1

    try:
        binary_path = resolve_binary(module_name)
    except Exception as exc:  # noqa: BLE001
        print(f"[error] Failed to load platform binary: {exc}")
        return 1

    if sys.platform != "win32":
        current_mode = binary_path.stat().st_mode
        binary_path.chmod(current_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    result = subprocess.run([str(binary_path), *sys.argv[1:]], check=False)
    return result.returncode


if __name__ == "__main__":
    raise SystemExit(main())
