FROM postgres:17.6-bookworm

ARG AGE_REF=release/PG17/1.7.0
ARG PGVECTOR_REF=v0.8.2

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bison \
        build-essential \
        ca-certificates \
        flex \
        git \
        postgresql-17-pgaudit \
        postgresql-server-dev-17 \
    && rm -rf /var/lib/apt/lists/*

RUN git clone --depth 1 --branch "${AGE_REF}" https://github.com/apache/age.git /tmp/age \
    && make -C /tmp/age PG_CONFIG=/usr/lib/postgresql/17/bin/pg_config install \
    && rm -rf /tmp/age

RUN git clone --depth 1 --branch "${PGVECTOR_REF}" https://github.com/pgvector/pgvector.git /tmp/pgvector \
    && make -C /tmp/pgvector PG_CONFIG=/usr/lib/postgresql/17/bin/pg_config install \
    && rm -rf /tmp/pgvector
