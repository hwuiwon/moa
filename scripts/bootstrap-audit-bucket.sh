#!/usr/bin/env bash
set -euo pipefail

: "${MOA_AUDIT_BUCKET:?MOA_AUDIT_BUCKET is required}"
: "${AWS_REGION:?AWS_REGION is required}"

MODE="${MOA_AUDIT_OBJECT_LOCK_MODE:-COMPLIANCE}"
YEARS="${MOA_AUDIT_RETENTION_YEARS:-10}"

aws s3api create-bucket \
  --bucket "${MOA_AUDIT_BUCKET}" \
  --region "${AWS_REGION}" \
  --object-lock-enabled-for-bucket

aws s3api put-bucket-versioning \
  --bucket "${MOA_AUDIT_BUCKET}" \
  --versioning-configuration Status=Enabled

aws s3api put-public-access-block \
  --bucket "${MOA_AUDIT_BUCKET}" \
  --public-access-block-configuration \
    BlockPublicAcls=true,IgnorePublicAcls=true,BlockPublicPolicy=true,RestrictPublicBuckets=true

aws s3api put-object-lock-configuration \
  --bucket "${MOA_AUDIT_BUCKET}" \
  --object-lock-configuration "{
    \"ObjectLockEnabled\": \"Enabled\",
    \"Rule\": {
      \"DefaultRetention\": {
        \"Mode\": \"${MODE}\",
        \"Years\": ${YEARS}
      }
    }
  }"
