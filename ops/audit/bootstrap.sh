#!/usr/bin/env bash
set -euo pipefail

ENV="${ENV:?ENV is required, for example dev or prod}"
REGION="${REGION:-us-east-1}"
BUCKET="${BUCKET:-moa-audit-${ENV}}"
RETENTION_DAYS="${RETENTION_DAYS:-2190}"

create_bucket_args=(--bucket "${BUCKET}" --region "${REGION}" --object-lock-enabled-for-bucket)
if [[ "${REGION}" != "us-east-1" ]]; then
  create_bucket_args+=(--create-bucket-configuration "LocationConstraint=${REGION}")
fi

aws s3api create-bucket "${create_bucket_args[@]}"
aws s3api put-bucket-versioning \
  --bucket "${BUCKET}" \
  --versioning-configuration Status=Enabled
aws s3api put-object-lock-configuration \
  --bucket "${BUCKET}" \
  --object-lock-configuration "{
    \"ObjectLockEnabled\":\"Enabled\",
    \"Rule\":{\"DefaultRetention\":{\"Mode\":\"COMPLIANCE\",\"Days\":${RETENTION_DAYS}}}
  }"
aws s3api put-public-access-block \
  --bucket "${BUCKET}" \
  --public-access-block-configuration '{
    "BlockPublicAcls":true,
    "IgnorePublicAcls":true,
    "BlockPublicPolicy":true,
    "RestrictPublicBuckets":true
  }'
