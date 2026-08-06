#!/usr/bin/env python3
"""Collect portable benchmark provenance and verify pinned fixture hashes."""

from __future__ import annotations

import argparse
import ctypes
import datetime as dt
import hashlib
import hmac
import json
import os
import platform
import subprocess
import sys
from pathlib import Path
from typing import Any, Sequence


def command(args: Sequence[str], *, cwd: Path | None = None) -> str | None:
    """Return stripped command output, or None when the probe is unavailable."""
    try:
        result = subprocess.run(
            args,
            cwd=cwd,
            check=False,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=15,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    output = result.stdout.strip()
    return output if result.returncode == 0 and output else None


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def linux_cpu() -> tuple[str | None, list[str]]:
    try:
        cpuinfo = Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
    except OSError:
        return None, []

    model_candidates: dict[str, str] = {}
    features: list[str] = []
    for line in cpuinfo.splitlines():
        key, separator, value = line.partition(":")
        if not separator:
            continue
        normalized = key.strip().lower()
        if normalized in {"model name", "hardware", "cpu model"}:
            model_candidates.setdefault(normalized, value.strip())
        if not features and normalized in {"flags", "features"}:
            features = value.split()
    model = next(
        (
            model_candidates[key]
            for key in ("model name", "hardware", "cpu model")
            if model_candidates.get(key)
        ),
        None,
    )
    return model, features


def macos_cpu() -> tuple[str | None, list[str]]:
    model = command(["sysctl", "-n", "machdep.cpu.brand_string"])
    if model is None:
        model = command(["sysctl", "-n", "hw.model"])

    features: set[str] = set()
    for key in (
        "machdep.cpu.features",
        "machdep.cpu.leaf7_features",
        "machdep.cpu.extfeatures",
    ):
        value = command(["sysctl", "-n", key])
        if value:
            features.update(value.lower().split())

    if platform.machine().lower() in {"arm64", "aarch64"}:
        optional = command(["sysctl", "-a"])
        if optional:
            for line in optional.splitlines():
                key, separator, value = line.partition(":")
                if (
                    separator
                    and key.startswith("hw.optional.")
                    and value.strip() == "1"
                ):
                    features.add(key.removeprefix("hw.optional."))
    return model, sorted(features)


def windows_cpu() -> tuple[str | None, list[str]]:
    try:
        import winreg

        with winreg.OpenKey(
            winreg.HKEY_LOCAL_MACHINE,
            r"HARDWARE\DESCRIPTION\System\CentralProcessor\0",
        ) as key:
            model = str(winreg.QueryValueEx(key, "ProcessorNameString")[0]).strip()
    except (ImportError, OSError):
        model = None
    return model, []


def cpu_metadata() -> dict[str, Any]:
    system = platform.system()
    if system == "Linux":
        model, features = linux_cpu()
    elif system == "Darwin":
        model, features = macos_cpu()
    elif system == "Windows":
        model, features = windows_cpu()
    else:
        model, features = None, []

    fallback = platform.processor().strip() or platform.machine().strip() or None
    return {
        "model": model or fallback,
        "architecture": platform.machine() or None,
        "logical_cpus": os.cpu_count(),
        "host_features": features,
    }


def windows_memory_bytes() -> int | None:
    class MemoryStatus(ctypes.Structure):
        _fields_ = [
            ("length", ctypes.c_ulong),
            ("memory_load", ctypes.c_ulong),
            ("total_physical", ctypes.c_ulonglong),
            ("available_physical", ctypes.c_ulonglong),
            ("total_page_file", ctypes.c_ulonglong),
            ("available_page_file", ctypes.c_ulonglong),
            ("total_virtual", ctypes.c_ulonglong),
            ("available_virtual", ctypes.c_ulonglong),
            ("available_extended_virtual", ctypes.c_ulonglong),
        ]

    status = MemoryStatus()
    status.length = ctypes.sizeof(MemoryStatus)
    try:
        success = ctypes.windll.kernel32.GlobalMemoryStatusEx(ctypes.byref(status))
    except (AttributeError, OSError):
        return None
    return int(status.total_physical) if success else None


def memory_bytes() -> int | None:
    system = platform.system()
    if system == "Linux":
        try:
            for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
                if line.startswith("MemTotal:"):
                    return int(line.split()[1]) * 1024
        except (OSError, ValueError, IndexError):
            pass
    elif system == "Darwin":
        value = command(["sysctl", "-n", "hw.memsize"])
        if value:
            try:
                return int(value)
            except ValueError:
                pass
    elif system == "Windows":
        return windows_memory_bytes()

    try:
        return int(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES"))
    except (AttributeError, OSError, ValueError):
        return None


def toml_section(path: Path, section: str) -> str | None:
    """Extract a small TOML section verbatim without adding a TOML dependency."""
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError:
        return None

    header = f"[{section}]"
    captured: list[str] = []
    active = False
    for line in lines:
        stripped = line.strip()
        if stripped == header:
            active = True
            captured.append(line)
            continue
        if active and stripped.startswith("["):
            break
        if active:
            captured.append(line)
    while captured and (
        not captured[-1].strip() or captured[-1].lstrip().startswith("#")
    ):
        captured.pop()
    value = "\n".join(captured).strip()
    return value or None


def lock_hashes(repository: Path) -> dict[str, str | None]:
    files = {
        "workspace": repository / "Cargo.lock",
        "rawler": repository / "thirdparty/dnglab/Cargo.lock",
        "jpeg_bakeoff": repository / "tools/jpeg-bakeoff/Cargo.lock",
    }
    return {
        name: sha256_file(path) if path.is_file() else None
        for name, path in files.items()
    }


def rust_target_features(target: str, *, native: bool = False) -> list[str]:
    rustc_command = ["rustc", "--print", "cfg", "--target", target]
    if native:
        rustc_command.extend(["-C", "target-cpu=native"])
    cfg = command(rustc_command) or ""
    prefix = 'target_feature="'
    return sorted(
        line[len(prefix) : -1]
        for line in cfg.splitlines()
        if line.startswith(prefix) and line.endswith('"')
    )


def profile_metadata(repository: Path, profile: str) -> dict[str, Any]:
    return {
        "name": profile,
        "workspace": toml_section(repository / "Cargo.toml", f"profile.{profile}"),
        "rawler_workspace": toml_section(
            repository / "thirdparty/dnglab/Cargo.toml", f"profile.{profile}"
        ),
        "jpeg_bakeoff": toml_section(
            repository / "tools/jpeg-bakeoff/Cargo.toml", f"profile.{profile}"
        ),
    }


def collect(args: argparse.Namespace) -> int:
    repository = Path(args.repository).resolve()
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)

    submodule_path = repository / "thirdparty/dnglab"
    metadata = {
        "schema_version": 3,
        "recorded_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "platform_label": args.platform,
        "scope": args.scope,
        "git": {
            "commit": command(["git", "rev-parse", "HEAD"], cwd=repository),
            "workflow_sha": os.environ.get("GITHUB_SHA"),
            "ref": os.environ.get("GITHUB_REF"),
            "submodules": command(
                ["git", "submodule", "status", "--recursive"], cwd=repository
            ),
            "rawler_commit": command(["git", "rev-parse", "HEAD"], cwd=submodule_path),
        },
        "runner": {
            "os": platform.system(),
            "os_release": platform.release(),
            "os_version": platform.version(),
            "platform": platform.platform(),
            "runner_os": os.environ.get("RUNNER_OS"),
            "runner_arch": os.environ.get("RUNNER_ARCH"),
            "runner_image_os": os.environ.get("ImageOS"),
            "runner_image_version": os.environ.get("ImageVersion"),
            "memory_bytes": memory_bytes(),
            "cpu": cpu_metadata(),
        },
        "build": {
            "target": args.target,
            "rustflags": args.rustflags,
            "cargo_encoded_rustflags": os.environ.get("CARGO_ENCODED_RUSTFLAGS"),
            "rust_target_features": rust_target_features(args.target),
            "rust_native_target_features": rust_target_features(
                args.target, native=True
            ),
            "profile": profile_metadata(repository, args.profile),
            "cargo_target_dir": os.environ.get("CARGO_TARGET_DIR"),
            "rayon_threads": os.environ.get("RAYON_NUM_THREADS", "default"),
            "rustc": command(["rustc", "-vV"]),
            "cargo": command(["cargo", "--version", "--verbose"]),
        },
        "locks_sha256": lock_hashes(repository),
        "criterion": {
            "cache_condition": "Criterion warm-up; operating-system cache not reset",
            "numeric_gate": False,
            "jpeg_auto_baseline": "jpeg-automatic",
            "jpeg_force_scalar_result": "new",
            "jpeg_criterion_scope": "jpeg_encode groups only; Viewr decode uses zune-jpeg",
            "jpeg_comparison_kind": "separate whole-build policy comparison",
            "raw_auto_baseline": "automatic-dispatch",
            "raw_force_baseline_result": "new",
            "raw_comparison_kind": "separate whole-build policy comparison",
        },
        "workflow": {
            "event": os.environ.get("GITHUB_EVENT_NAME"),
            "run_id": os.environ.get("GITHUB_RUN_ID"),
            "run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
        },
    }

    output.write_text(
        json.dumps(metadata, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    if args.markdown_output:
        markdown_output = Path(args.markdown_output)
        markdown_output.parent.mkdir(parents=True, exist_ok=True)
        cpu = metadata["runner"]["cpu"]
        rows = [
            ("Platform", args.platform),
            ("Scope", args.scope),
            ("OS", metadata["runner"]["platform"]),
            ("CPU", cpu["model"]),
            ("Logical CPUs", cpu["logical_cpus"]),
            ("Memory bytes", metadata["runner"]["memory_bytes"]),
            ("Target", args.target),
            ("RUSTFLAGS", args.rustflags or "(none)"),
            (
                "rustc native target features",
                ", ".join(metadata["build"]["rust_native_target_features"])
                or "(none reported)",
            ),
            ("Profile", args.profile),
            ("Commit", metadata["git"]["commit"]),
            ("Rawler commit", metadata["git"]["rawler_commit"]),
        ]

        def markdown_cell(value: Any) -> str:
            return (
                str(value if value is not None else "unknown")
                .replace("|", "\\|")
                .replace("\n", "<br>")
            )

        markdown = [
            f"## Advisory benchmark environment: {markdown_cell(args.platform)}",
            "",
            "| Field | Value |",
            "| --- | --- |",
            *(
                f"| {markdown_cell(key)} | {markdown_cell(value)} |"
                for key, value in rows
            ),
            "",
            "Measurements are advisory; this workflow has no numeric performance gate.",
            "",
        ]
        markdown_output.write_text("\n".join(markdown), encoding="utf-8")

    print(f"wrote benchmark metadata to {output}")
    return 0


def verify_sha256(args: argparse.Namespace) -> int:
    path = Path(args.path)
    if not path.is_file():
        print(f"error: fixture does not exist: {path}", file=sys.stderr)
        return 1
    actual = sha256_file(path)
    expected = args.expected.lower()
    if not hmac.compare_digest(actual, expected):
        print(
            f"error: SHA-256 mismatch for {path}: expected {expected}, got {actual}",
            file=sys.stderr,
        )
        return 1
    print(f"verified SHA-256 {actual}  {path}")
    return 0


def parser() -> argparse.ArgumentParser:
    root = argparse.ArgumentParser(description=__doc__)
    subcommands = root.add_subparsers(dest="command", required=True)

    collect_parser = subcommands.add_parser("collect", help="write benchmark metadata")
    collect_parser.add_argument("--output", required=True)
    collect_parser.add_argument("--markdown-output")
    collect_parser.add_argument("--platform", required=True)
    collect_parser.add_argument("--scope", required=True)
    collect_parser.add_argument("--target", required=True)
    collect_parser.add_argument("--rustflags", default="")
    collect_parser.add_argument("--profile", default="bench")
    collect_parser.add_argument("--repository", default=".")
    collect_parser.set_defaults(handler=collect)

    verify_parser = subcommands.add_parser("verify-sha256", help="verify a pinned file")
    verify_parser.add_argument("path")
    verify_parser.add_argument("expected")
    verify_parser.set_defaults(handler=verify_sha256)
    return root


def main() -> int:
    args = parser().parse_args()
    return int(args.handler(args))


if __name__ == "__main__":
    raise SystemExit(main())
