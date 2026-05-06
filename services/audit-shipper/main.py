"""Ship completed PostgreSQL audit logs to S3 Object Lock."""

from __future__ import annotations

import gzip
import hashlib
import logging
import os
import shutil
import time
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

import boto3
from boto3.s3.transfer import TransferConfig


LOGGER = logging.getLogger("moa.audit_shipper")
MULTIPART_THRESHOLD_BYTES = 100 * 1024 * 1024
ACTIVE_LOG_WINDOW_SECONDS = 5


@dataclass(frozen=True)
class Settings:
    """Runtime settings for the audit shipper."""

    log_dir: Path
    state_dir: Path
    bucket: str
    region: str
    object_lock_days: int
    poll_interval_seconds: int
    quiet_seconds: int
    log_globs: tuple[str, ...]
    kms_key_id: str | None


def load_settings() -> Settings:
    """Loads shipper settings from environment variables."""

    bucket = os.environ.get("BUCKET")
    if not bucket:
        raise SystemExit("BUCKET is required")

    globs = tuple(
        item.strip()
        for item in os.environ.get("LOG_GLOB", "*.log,*.csv").split(",")
        if item.strip()
    )
    return Settings(
        log_dir=Path(os.environ.get("LOG_DIR", "/var/log/postgresql")),
        state_dir=Path(os.environ.get("STATE_DIR", "/var/lib/moa-audit-shipper")),
        bucket=bucket,
        region=os.environ.get("AWS_REGION", "us-east-1"),
        object_lock_days=int(os.environ.get("OBJECT_LOCK_DAYS", "2190")),
        poll_interval_seconds=int(os.environ.get("POLL_INTERVAL_SECONDS", "60")),
        quiet_seconds=int(os.environ.get("QUIET_SECONDS", "120")),
        log_globs=globs,
        kms_key_id=os.environ.get("SSE_KMS_KEY_ID"),
    )


def completed_logs(settings: Settings, now: float) -> list[Path]:
    """Returns log files that are stable enough to upload."""

    files: dict[Path, None] = {}
    for pattern in settings.log_globs:
        for path in settings.log_dir.glob(pattern):
            if path.is_file() and not path.name.endswith(".gz"):
                files[path] = None

    latest_mtime = max((path.stat().st_mtime for path in files), default=0.0)
    stable = []
    for path in sorted(files):
        stat = path.stat()
        if (
            stat.st_size > 0
            and now - stat.st_mtime >= settings.quiet_seconds
            and latest_mtime - stat.st_mtime > ACTIVE_LOG_WINDOW_SECONDS
            and not shipped_marker(settings, path).exists()
        ):
            stable.append(path)
    return stable


def shipped_marker(settings: Settings, source: Path) -> Path:
    """Returns the state marker path for one source file version."""

    stat = source.stat()
    fingerprint = hashlib.sha256(
        f"{source}:{stat.st_size}:{stat.st_mtime_ns}".encode("utf-8")
    ).hexdigest()
    return settings.state_dir / "shipped" / f"{source.name}.{fingerprint}.done"


def compressed_copy(source: Path, state_dir: Path) -> Path:
    """Writes a gzip copy of a PostgreSQL log file into the state directory."""

    state_dir.mkdir(parents=True, exist_ok=True)
    destination = state_dir / f"{source.name}.gz"
    temporary = destination.with_suffix(f"{destination.suffix}.tmp")
    with source.open("rb") as source_file, gzip.open(temporary, "wb") as gzip_file:
        shutil.copyfileobj(source_file, gzip_file)
    temporary.replace(destination)
    return destination


def object_key(source: Path, now: datetime) -> str:
    """Builds the S3 key for one compressed audit log segment."""

    return (
        "workspace=unknown/"
        f"year={now.year:04d}/"
        f"month={now.month:02d}/"
        f"{source.name}.gz"
    )


def upload_args(settings: Settings, now: datetime) -> dict[str, object]:
    """Builds S3 upload arguments for Object Lock COMPLIANCE retention."""

    retain_until = now + timedelta(days=settings.object_lock_days)
    args: dict[str, object] = {
        "ObjectLockMode": "COMPLIANCE",
        "ObjectLockRetainUntilDate": retain_until,
        "ServerSideEncryption": "aws:kms",
    }
    if settings.kms_key_id:
        args["SSEKMSKeyId"] = settings.kms_key_id
    return args


def upload_file(settings: Settings, source: Path, compressed: Path, now: datetime) -> None:
    """Uploads one compressed audit log to S3 with Object Lock retention."""

    client = boto3.client("s3", region_name=settings.region)
    client.upload_file(
        str(compressed),
        settings.bucket,
        object_key(source, now),
        ExtraArgs=upload_args(settings, now),
        Config=TransferConfig(
            multipart_threshold=MULTIPART_THRESHOLD_BYTES,
            multipart_chunksize=64 * 1024 * 1024,
        ),
    )


def ship_once(settings: Settings) -> int:
    """Uploads all currently completed log files and returns the count."""

    now_seconds = time.time()
    now_datetime = datetime.now(UTC)
    shipped = 0
    for source in completed_logs(settings, now_seconds):
        marker = shipped_marker(settings, source)
        compressed = compressed_copy(source, settings.state_dir)
        upload_file(settings, source, compressed, now_datetime)
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(f"shipped_at={now_datetime.isoformat()}\n", encoding="utf-8")
        compressed.unlink(missing_ok=True)
        shipped += 1
        LOGGER.info("shipped audit log", extra={"path": str(source)})
    return shipped


def main() -> None:
    """Runs the audit shipper loop."""

    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO"))
    settings = load_settings()
    LOGGER.info("starting audit shipper", extra={"bucket": settings.bucket})
    while True:
        try:
            ship_once(settings)
        except Exception:
            LOGGER.exception("audit shipping pass failed")
        time.sleep(settings.poll_interval_seconds)


if __name__ == "__main__":
    main()
