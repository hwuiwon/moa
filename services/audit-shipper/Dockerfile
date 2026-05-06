FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    LOG_DIR=/var/log/postgresql \
    STATE_DIR=/var/lib/moa-audit-shipper \
    POLL_INTERVAL_SECONDS=60 \
    OBJECT_LOCK_DAYS=2190

WORKDIR /app

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY services/audit-shipper/requirements.txt /app/requirements.txt
RUN pip install --no-cache-dir -r /app/requirements.txt

COPY services/audit-shipper/main.py /app/main.py

CMD ["python", "/app/main.py"]
