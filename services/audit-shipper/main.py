"""Ship completed PostgreSQL audit logs to S3 Object Lock."""

from __future__ import annotations

import gzip
import hashlib
import io
import logging
import os
import shutil
import time
from dataclasses import dataclass
from datetime import UTC, datetime, timedelta
from pathlib import Path

import boto3
import psycopg
from psycopg.rows import dict_row
from boto3.s3.transfer import TransferConfig


LOGGER = logging.getLogger("moa.audit_shipper")
MULTIPART_THRESHOLD_BYTES = 100 * 1024 * 1024
ACTIVE_LOG_WINDOW_SECONDS = 5
SECURITY_BATCH_SIZE = 500
ASSUME_ROLE_CACHE: dict[str, tuple[object, datetime]] = {}


@dataclass(frozen=True)
class Settings:
    """Runtime settings for the audit shipper."""

    log_dir: Path
    state_dir: Path
    bucket: str | None
    region: str
    object_lock_days: int
    poll_interval_seconds: int
    quiet_seconds: int
    log_globs: tuple[str, ...]
    kms_key_id: str | None
    postgres_url: str | None
    security_batch_size: int


@dataclass(frozen=True)
class AuditDestination:
    """Per-tenant security-event S3 destination."""

    tenant_id: str
    bucket_name: str
    region: str
    assume_role_arn: str | None
    key_prefix: str
    object_lock_days: int
    encryption_kms_key_arn: str | None


def load_settings() -> Settings:
    """Loads shipper settings from environment variables."""

    bucket = os.environ.get("BUCKET")
    postgres_url = os.environ.get("MOA_DATABASE_URL")
    if not bucket and not postgres_url:
        raise SystemExit("BUCKET or MOA_DATABASE_URL is required")

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
        postgres_url=postgres_url,
        security_batch_size=int(os.environ.get("SECURITY_BATCH_SIZE", str(SECURITY_BATCH_SIZE))),
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

    if not settings.bucket:
        return
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
    if settings.bucket:
        sources = completed_logs(settings, now_seconds)
    else:
        sources = []
    for source in sources:
        marker = shipped_marker(settings, source)
        compressed = compressed_copy(source, settings.state_dir)
        upload_file(settings, source, compressed, now_datetime)
        marker.parent.mkdir(parents=True, exist_ok=True)
        marker.write_text(f"shipped_at={now_datetime.isoformat()}\n", encoding="utf-8")
        compressed.unlink(missing_ok=True)
        shipped += 1
        LOGGER.info("shipped audit log", extra={"path": str(source)})
    shipped += ship_security_events_once(settings)
    return shipped


def ship_security_events_once(settings: Settings) -> int:
    """Ships unshipped OCSF security events to tenant audit buckets."""

    if not settings.postgres_url:
        return 0
    with psycopg.connect(settings.postgres_url, row_factory=dict_row) as conn:
        with conn.cursor() as cur:
            cur.execute(
                """
                SELECT id, tenant_id, class_uid, event_jcs, occurred_at
                FROM security_events
                WHERE shipped_at IS NULL
                ORDER BY tenant_id, id
                LIMIT %s
                """,
                (settings.security_batch_size,),
            )
            events = list(cur.fetchall())
            if not events:
                return 0
            tenant_ids = sorted({str(event["tenant_id"]) for event in events})
            destinations = load_destinations(cur, tenant_ids)
            shipped = 0
            for tenant_id in tenant_ids:
                tenant_events = [event for event in events if str(event["tenant_id"]) == tenant_id]
                destination = destinations.get(tenant_id)
                if destination is None:
                    mark_security_ship_failure(
                        cur,
                        [event["id"] for event in tenant_events],
                        "tenant audit destination is not configured",
                    )
                    continue
                try:
                    upload_security_batch(destination, tenant_events)
                except Exception as exc:
                    LOGGER.exception(
                        "security event batch upload failed",
                        extra={"tenant_id": tenant_id},
                    )
                    mark_security_ship_failure(
                        cur,
                        [event["id"] for event in tenant_events],
                        str(exc),
                    )
                    continue
                cur.execute(
                    "UPDATE security_events SET shipped_at = NOW(), last_ship_error = NULL WHERE id = ANY(%s::uuid[])",
                    ([event["id"] for event in tenant_events],),
                )
                shipped += len(tenant_events)
                LOGGER.info(
                    "shipped security events",
                    extra={"tenant_id": tenant_id, "count": len(tenant_events)},
                )
            conn.commit()
            return shipped


def load_destinations(cur: psycopg.Cursor, tenant_ids: list[str]) -> dict[str, AuditDestination]:
    """Loads configured audit destinations for tenant ids."""

    cur.execute(
        """
        SELECT tenant_id, bucket_name, region, assume_role_arn, key_prefix,
               object_lock_days, encryption_kms_key_arn
        FROM tenant_audit_destinations
        WHERE tenant_id = ANY(%s::uuid[])
        """,
        (tenant_ids,),
    )
    destinations = {}
    for row in cur.fetchall():
        tenant_id = str(row["tenant_id"])
        destinations[tenant_id] = AuditDestination(
            tenant_id=tenant_id,
            bucket_name=row["bucket_name"],
            region=row["region"],
            assume_role_arn=row["assume_role_arn"],
            key_prefix=row["key_prefix"],
            object_lock_days=row["object_lock_days"],
            encryption_kms_key_arn=row["encryption_kms_key_arn"],
        )
    return destinations


def upload_security_batch(destination: AuditDestination, events: list[dict[str, object]]) -> None:
    """Uploads one tenant's security event batch as gzipped NDJSON."""

    if not events:
        return
    first = events[0]
    occurred_at = first["occurred_at"]
    if not isinstance(occurred_at, datetime):
        occurred_at = datetime.now(UTC)
    body = io.BytesIO()
    with gzip.GzipFile(fileobj=body, mode="wb") as gzip_file:
        for event in events:
            payload = bytes(event["event_jcs"])
            gzip_file.write(payload)
            gzip_file.write(b"\n")
    key = security_object_key(destination, occurred_at, str(first["id"]))
    client = s3_client_for_destination(destination)
    retain_until = datetime.now(UTC) + timedelta(days=destination.object_lock_days)
    put_args: dict[str, object] = {
        "Bucket": destination.bucket_name,
        "Key": key,
        "Body": body.getvalue(),
        "ObjectLockMode": "COMPLIANCE",
        "ObjectLockRetainUntilDate": retain_until,
    }
    if destination.encryption_kms_key_arn:
        put_args["ServerSideEncryption"] = "aws:kms"
        put_args["SSEKMSKeyId"] = destination.encryption_kms_key_arn
    client.put_object(**put_args)


def security_object_key(destination: AuditDestination, occurred_at: datetime, first_id: str) -> str:
    """Builds the S3 object key for one OCSF batch."""

    prefix = destination.key_prefix.strip("/")
    return (
        f"{prefix}/"
        f"{occurred_at.year:04d}/"
        f"{occurred_at.month:02d}/"
        f"{occurred_at.day:02d}/"
        f"{occurred_at.hour:02d}/"
        f"{first_id}.jsonl.gz"
    )


def s3_client_for_destination(destination: AuditDestination):
    """Builds or reuses an S3 client for a tenant destination."""

    if not destination.assume_role_arn:
        return boto3.client("s3", region_name=destination.region)
    cache_key = f"{destination.assume_role_arn}:{destination.region}"
    cached = ASSUME_ROLE_CACHE.get(cache_key)
    if cached and cached[1] > datetime.now(UTC) + timedelta(minutes=5):
        return cached[0]
    sts = boto3.client("sts", region_name=destination.region)
    assumed = sts.assume_role(
        RoleArn=destination.assume_role_arn,
        RoleSessionName=f"moa-audit-{destination.tenant_id[:8]}",
    )
    credentials = assumed["Credentials"]
    client = boto3.client(
        "s3",
        region_name=destination.region,
        aws_access_key_id=credentials["AccessKeyId"],
        aws_secret_access_key=credentials["SecretAccessKey"],
        aws_session_token=credentials["SessionToken"],
    )
    ASSUME_ROLE_CACHE[cache_key] = (client, credentials["Expiration"])
    return client


def mark_security_ship_failure(cur: psycopg.Cursor, event_ids: list[object], error: str) -> None:
    """Records security-event shipping failures for retry."""

    cur.execute(
        """
        UPDATE security_events
        SET ship_attempts = ship_attempts + 1,
            last_ship_error = %s
        WHERE id = ANY(%s::uuid[])
        """,
        (error[:2000], event_ids),
    )


def main() -> None:
    """Runs the audit shipper loop."""

    logging.basicConfig(level=os.environ.get("LOG_LEVEL", "INFO"))
    settings = load_settings()
    LOGGER.info(
        "starting audit shipper",
        extra={"bucket": settings.bucket, "security_events": bool(settings.postgres_url)},
    )
    while True:
        try:
            ship_once(settings)
        except Exception:
            LOGGER.exception("audit shipping pass failed")
        time.sleep(settings.poll_interval_seconds)


if __name__ == "__main__":
    main()
