#!/usr/bin/env python3
"""WixQA vector quantization oracle for embedding export bundles."""

from __future__ import annotations

import argparse
import json
import math
import os
import time
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np


COHERE_EMBED_URL = "https://api.cohere.com/v2/embed"
COHERE_MAX_TEXTS = 96
COHERE_INPUTS_PER_MINUTE = 1_900


def main() -> int:
    args = parse_args()
    if args.self_test:
        return run_self_test(args)
    if args.input is None:
        raise SystemExit("--input is required unless --self-test is set")
    bundle = load_bundle(args.input)
    report = run_oracle(bundle, args)
    if args.output:
        write_json(args.output, report)
    else:
        print(json.dumps(report, indent=2))
    print_summary(report, args.top_k)
    return 0


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare float, f16, post-hoc int8, and optional Cohere-native int8 retrieval."
    )
    parser.add_argument("--input", type=Path, help="Path to a WixQA embedding export JSON bundle")
    parser.add_argument("--output", type=Path, help="Path for the quantization report JSON")
    parser.add_argument("--top-k", type=int, default=25, help="Article-level metric cutoff")
    parser.add_argument(
        "--native-cohere-int8",
        action="store_true",
        help="Call Cohere Embed v4 for native float+int8 embeddings using bundle text fields",
    )
    parser.add_argument(
        "--cohere-api-key-env",
        default="MOA_COHERE_API_KEY",
        help="Environment variable containing the Cohere API key",
    )
    parser.add_argument("--cohere-model", default="embed-v4.0", help="Cohere embedding model")
    parser.add_argument("--cohere-timeout", type=float, default=60.0, help="HTTP timeout seconds")
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run a deterministic tiny fixture without reading --input",
    )
    args = parser.parse_args()
    if args.top_k <= 0:
        raise SystemExit("--top-k must be greater than 0")
    return args


def run_self_test(args: argparse.Namespace) -> int:
    bundle = {
        "dataset": "self-test",
        "cache_key": "self-test",
        "embedding_model": "fixture",
        "embedding_dimensions": 3,
        "metric": "cosine",
        "chunks": [
            {
                "uid": "00000000-0000-0000-0000-000000000001",
                "article_id": "article-a",
                "title": "A",
                "source_uri": "https://example.invalid/a",
                "text": "alpha",
                "embedding": [1.0, 0.0, 0.0],
            },
            {
                "uid": "00000000-0000-0000-0000-000000000002",
                "article_id": "article-b",
                "title": "B",
                "source_uri": "https://example.invalid/b",
                "text": "beta",
                "embedding": [0.0, 1.0, 0.0],
            },
        ],
        "queries": [
            {
                "question": "find A",
                "gold_article_ids": ["article-a"],
                "embedding": [0.9, 0.1, 0.0],
            },
            {
                "question": "find B",
                "gold_article_ids": ["article-b"],
                "embedding": [0.1, 0.9, 0.0],
            },
        ],
    }
    args.native_cohere_int8 = False
    report = run_oracle(bundle, args)
    for profile_name, profile in report["profiles"].items():
        metrics = profile["metrics"]
        for metric_name in ["recall_at_k", "hit_at_k", "mrr", "ndcg_at_k"]:
            if not math.isclose(metrics[metric_name], 1.0, rel_tol=0.0, abs_tol=1e-12):
                raise SystemExit(
                    f"self-test failed: {profile_name} {metric_name}={metrics[metric_name]}"
                )
    if args.output:
        write_json(args.output, report)
    print(f"self-test passed: profiles={','.join(report['profiles'].keys())}")
    return 0


def load_bundle(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as handle:
        bundle = json.load(handle)
    if not bundle.get("chunks"):
        raise SystemExit(f"{path} has no chunks")
    if not bundle.get("queries"):
        raise SystemExit(f"{path} has no queries")
    return bundle


def run_oracle(bundle: dict[str, Any], args: argparse.Namespace) -> dict[str, Any]:
    chunks = bundle["chunks"]
    queries = bundle["queries"]
    dimension = int(bundle["embedding_dimensions"])
    chunk_matrix = matrix_from_records(chunks, "chunks", dimension)
    query_matrix = matrix_from_records(queries, "queries", dimension)

    profiles = [
        profile_float32_cosine(chunks, queries, chunk_matrix, query_matrix, args.top_k),
        profile_float16_cosine(chunks, queries, chunk_matrix, query_matrix, args.top_k),
        profile_posthoc_int8_cosine(chunks, queries, chunk_matrix, query_matrix, args.top_k),
        profile_posthoc_int8_dot(chunks, queries, chunk_matrix, query_matrix, args.top_k),
    ]
    if args.native_cohere_int8:
        profiles.extend(
            cohere_native_profiles(
                chunks,
                queries,
                dimension,
                args.top_k,
                args.cohere_model,
                args.cohere_api_key_env,
                args.cohere_timeout,
            )
        )

    baseline = profiles[0]
    return {
        "dataset": bundle.get("dataset"),
        "cache_key": bundle.get("cache_key"),
        "source_embedding_model": bundle.get("embedding_model"),
        "embedding_dimensions": dimension,
        "top_k": args.top_k,
        "chunk_count": len(chunks),
        "query_count": len(queries),
        "profiles": {
            profile.name: profile.to_json(baseline, dimension, len(chunks), len(queries))
            for profile in profiles
        },
        "notes": [
            "posthoc-int8 profiles quantize existing float vectors; Cohere-native profiles call the Embed API with embedding_types=[float,int8]",
            "native-cohere-int8-cosine is the closest match for Turbopuffer cosine_distance over [N]i8 vectors",
            "native-cohere-int8-dot follows Cohere's semantic-search tutorial scoring, but Turbopuffer write docs currently list cosine/euclidean distance metrics",
        ],
    }


@dataclass
class ProfileReport:
    name: str
    kind: str
    scoring: str
    bytes_per_dimension: int
    metrics: dict[str, float]
    latency: dict[str, float]
    per_query: list[dict[str, Any]]

    def to_json(
        self,
        baseline: "ProfileReport",
        dimension: int,
        chunk_count: int,
        query_count: int,
    ) -> dict[str, Any]:
        chunk_vector_bytes = dimension * chunk_count * self.bytes_per_dimension
        query_vector_bytes = dimension * query_count * self.bytes_per_dimension
        baseline_chunk_bytes = dimension * chunk_count * baseline.bytes_per_dimension
        return {
            "kind": self.kind,
            "scoring": self.scoring,
            "bytes_per_dimension": self.bytes_per_dimension,
            "estimated_vector_bytes": {
                "chunks": chunk_vector_bytes,
                "queries": query_vector_bytes,
                "chunk_ratio_vs_float32": safe_ratio(chunk_vector_bytes, baseline_chunk_bytes),
            },
            "metrics": self.metrics,
            "quality_delta_vs_float32": metric_delta(self.metrics, baseline.metrics),
            "rank_delta_vs_float32": rank_delta(self.per_query, baseline.per_query),
            "search_latency": self.latency,
            "per_query": self.per_query,
        }


def profile_float32_cosine(
    chunks: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    chunk_matrix: np.ndarray,
    query_matrix: np.ndarray,
    top_k: int,
) -> ProfileReport:
    chunk_vectors = l2_normalize(chunk_matrix.astype(np.float32, copy=True))
    query_vectors = l2_normalize(query_matrix.astype(np.float32, copy=True))
    return evaluate_profile(
        "float32-cosine", "float32", "cosine", 4, chunks, queries, chunk_vectors, query_vectors, top_k
    )


def profile_float16_cosine(
    chunks: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    chunk_matrix: np.ndarray,
    query_matrix: np.ndarray,
    top_k: int,
) -> ProfileReport:
    chunk_vectors = l2_normalize(chunk_matrix.astype(np.float16).astype(np.float32))
    query_vectors = l2_normalize(query_matrix.astype(np.float16).astype(np.float32))
    return evaluate_profile(
        "float16-cosine", "float16", "cosine", 2, chunks, queries, chunk_vectors, query_vectors, top_k
    )


def profile_posthoc_int8_cosine(
    chunks: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    chunk_matrix: np.ndarray,
    query_matrix: np.ndarray,
    top_k: int,
) -> ProfileReport:
    chunk_vectors = l2_normalize(rowwise_int8(chunk_matrix).astype(np.float32))
    query_vectors = l2_normalize(rowwise_int8(query_matrix).astype(np.float32))
    return evaluate_profile(
        "posthoc-int8-cosine",
        "posthoc-int8",
        "cosine",
        1,
        chunks,
        queries,
        chunk_vectors,
        query_vectors,
        top_k,
    )


def profile_posthoc_int8_dot(
    chunks: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    chunk_matrix: np.ndarray,
    query_matrix: np.ndarray,
    top_k: int,
) -> ProfileReport:
    chunk_vectors = rowwise_int8(chunk_matrix).astype(np.float32)
    query_vectors = rowwise_int8(query_matrix).astype(np.float32)
    return evaluate_profile(
        "posthoc-int8-dot",
        "posthoc-int8",
        "dot",
        1,
        chunks,
        queries,
        chunk_vectors,
        query_vectors,
        top_k,
    )


def cohere_native_profiles(
    chunks: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    dimension: int,
    top_k: int,
    model: str,
    api_key_env: str,
    timeout: float,
) -> list[ProfileReport]:
    api_key = os.environ.get(api_key_env, "").strip()
    if not api_key:
        raise SystemExit(f"{api_key_env} must be set for --native-cohere-int8")
    chunk_texts = []
    for index, chunk in enumerate(chunks):
        text = chunk.get("text")
        if not isinstance(text, str) or not text.strip():
            raise SystemExit(f"chunk[{index}] is missing non-empty text for Cohere native int8")
        chunk_texts.append(text)
    query_texts = [query["question"] for query in queries]

    pacer = InputPacer(COHERE_INPUTS_PER_MINUTE)
    doc_embeddings = cohere_embed(
        chunk_texts, "search_document", ["float", "int8"], model, dimension, api_key, timeout, pacer
    )
    query_embeddings = cohere_embed(
        query_texts, "search_query", ["float", "int8"], model, dimension, api_key, timeout, pacer
    )
    native_float_chunks = np.asarray(doc_embeddings["float"], dtype=np.float32)
    native_float_queries = np.asarray(query_embeddings["float"], dtype=np.float32)
    native_int8_chunks = np.asarray(doc_embeddings["int8"], dtype=np.int16)
    native_int8_queries = np.asarray(query_embeddings["int8"], dtype=np.int16)
    validate_matrix(native_float_chunks, "native float chunks", dimension)
    validate_matrix(native_float_queries, "native float queries", dimension)
    validate_matrix(native_int8_chunks, "native int8 chunks", dimension)
    validate_matrix(native_int8_queries, "native int8 queries", dimension)

    return [
        evaluate_profile(
            "native-cohere-float-cosine",
            "cohere-native-float",
            "cosine",
            4,
            chunks,
            queries,
            l2_normalize(native_float_chunks),
            l2_normalize(native_float_queries),
            top_k,
        ),
        evaluate_profile(
            "native-cohere-int8-cosine",
            "cohere-native-int8",
            "cosine",
            1,
            chunks,
            queries,
            l2_normalize(native_int8_chunks.astype(np.float32)),
            l2_normalize(native_int8_queries.astype(np.float32)),
            top_k,
        ),
        evaluate_profile(
            "native-cohere-int8-dot",
            "cohere-native-int8",
            "dot",
            1,
            chunks,
            queries,
            native_int8_chunks.astype(np.float32),
            native_int8_queries.astype(np.float32),
            top_k,
        ),
    ]


def cohere_embed(
    texts: list[str],
    input_type: str,
    embedding_types: list[str],
    model: str,
    dimension: int,
    api_key: str,
    timeout: float,
    pacer: "InputPacer",
) -> dict[str, list[list[float]]]:
    collected: dict[str, list[list[float]]] = {embedding_type: [] for embedding_type in embedding_types}
    for chunk in chunked(texts, COHERE_MAX_TEXTS):
        pacer.acquire(len(chunk))
        body = {
            "model": model,
            "texts": chunk,
            "input_type": input_type,
            "embedding_types": embedding_types,
            "output_dimension": dimension,
        }
        request = urllib.request.Request(
            COHERE_EMBED_URL,
            data=json.dumps(body).encode("utf-8"),
            headers={
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
            },
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=timeout) as response:
                payload = json.loads(response.read().decode("utf-8"))
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace")
            raise SystemExit(f"Cohere embed HTTP {error.code}: {detail}") from error
        embeddings = payload.get("embeddings", {})
        for embedding_type in embedding_types:
            values = embeddings.get(embedding_type)
            if not isinstance(values, list):
                raise SystemExit(f"Cohere response omitted embeddings.{embedding_type}")
            if len(values) != len(chunk):
                raise SystemExit(
                    f"Cohere {embedding_type} count mismatch: got {len(values)}, expected {len(chunk)}"
                )
            collected[embedding_type].extend(values)
    return collected


class InputPacer:
    def __init__(self, inputs_per_minute: int) -> None:
        self.inputs_per_minute = inputs_per_minute
        self.window_started = time.monotonic()
        self.inputs_used = 0

    def acquire(self, count: int) -> None:
        now = time.monotonic()
        elapsed = now - self.window_started
        if elapsed >= 60.0:
            self.window_started = now
            self.inputs_used = 0
        if self.inputs_used + count <= self.inputs_per_minute:
            self.inputs_used += count
            return
        sleep_seconds = max(0.0, 60.0 - elapsed)
        time.sleep(sleep_seconds)
        self.window_started = time.monotonic()
        self.inputs_used = count


def evaluate_profile(
    name: str,
    kind: str,
    scoring: str,
    bytes_per_dimension: int,
    chunks: list[dict[str, Any]],
    queries: list[dict[str, Any]],
    chunk_vectors: np.ndarray,
    query_vectors: np.ndarray,
    top_k: int,
) -> ProfileReport:
    per_query = []
    latencies = []
    search_k = len(chunks)
    for query_index, query in enumerate(queries):
        started = time.perf_counter()
        scores = chunk_vectors @ query_vectors[query_index]
        indices = np.argsort(-scores, kind="stable")[:search_k]
        latencies.append((time.perf_counter() - started) * 1000.0)
        ranked_articles, ranked_chunks = collapse_chunks_to_articles(chunks, indices, scores, top_k)
        metrics = query_metrics(ranked_articles, query["gold_article_ids"], top_k)
        per_query.append(
            {
                "question": query["question"],
                "gold_article_ids": query["gold_article_ids"],
                "ranked_article_ids": ranked_articles,
                "top_chunks": ranked_chunks,
                "metrics": metrics,
                "search_ms": latencies[-1],
            }
        )
    return ProfileReport(
        name=name,
        kind=kind,
        scoring=scoring,
        bytes_per_dimension=bytes_per_dimension,
        metrics=aggregate_metrics([row["metrics"] for row in per_query]),
        latency=latency_summary(latencies),
        per_query=per_query,
    )


def matrix_from_records(records: list[dict[str, Any]], name: str, dimension: int) -> np.ndarray:
    vectors = []
    for index, record in enumerate(records):
        vector = record.get("embedding")
        if not isinstance(vector, list):
            raise SystemExit(f"{name}[{index}] has no embedding list")
        if len(vector) != dimension:
            raise SystemExit(
                f"{name}[{index}] embedding has {len(vector)} dimensions, expected {dimension}"
            )
        vectors.append(vector)
    matrix = np.asarray(vectors, dtype=np.float32)
    validate_matrix(matrix, name, dimension)
    return matrix


def validate_matrix(matrix: np.ndarray, name: str, dimension: int) -> None:
    if matrix.ndim != 2 or matrix.shape[1] != dimension:
        raise SystemExit(f"{name} has shape {matrix.shape}, expected (*, {dimension})")


def l2_normalize(matrix: np.ndarray) -> np.ndarray:
    matrix = matrix.astype(np.float32, copy=False)
    norms = np.linalg.norm(matrix, axis=1, keepdims=True)
    norms[norms == 0.0] = 1.0
    return matrix / norms


def rowwise_int8(matrix: np.ndarray) -> np.ndarray:
    normalized = l2_normalize(matrix)
    max_abs = np.max(np.abs(normalized), axis=1, keepdims=True)
    max_abs[max_abs == 0.0] = 1.0
    scaled = np.rint(normalized / max_abs * 127.0)
    return np.clip(scaled, -128, 127).astype(np.int8)


def collapse_chunks_to_articles(
    chunks: list[dict[str, Any]],
    indices: np.ndarray,
    scores: np.ndarray,
    top_k: int,
) -> tuple[list[str], list[dict[str, Any]]]:
    ranked_articles = []
    ranked_chunks = []
    seen_articles = set()
    for index in indices.tolist():
        if index < 0:
            continue
        chunk = chunks[index]
        article_id = chunk["article_id"]
        if article_id in seen_articles:
            continue
        seen_articles.add(article_id)
        ranked_articles.append(article_id)
        ranked_chunks.append(
            {
                "uid": chunk["uid"],
                "article_id": article_id,
                "title": chunk.get("title"),
                "source_uri": chunk.get("source_uri"),
                "score": float(scores[index]),
            }
        )
        if len(ranked_articles) >= top_k:
            break
    return ranked_articles, ranked_chunks


def query_metrics(
    ranked_article_ids: list[str], gold_article_ids: list[str], top_k: int
) -> dict[str, Any]:
    gold = set(gold_article_ids)
    top_articles = ranked_article_ids[:top_k]
    matched = sum(1 for article_id in top_articles if article_id in gold)
    first_rank = None
    for index, article_id in enumerate(top_articles):
        if article_id in gold:
            first_rank = index + 1
            break
    dcg = sum(
        1.0 / math.log2(index + 2)
        for index, article_id in enumerate(top_articles)
        if article_id in gold
    )
    ideal_len = min(len(gold_article_ids), top_k)
    idcg = sum(1.0 / math.log2(index + 2) for index in range(ideal_len))
    return {
        "hit": matched > 0,
        "recall": 0.0 if not gold_article_ids else matched / len(gold_article_ids),
        "mrr": 0.0 if first_rank is None else 1.0 / first_rank,
        "ndcg": 0.0 if idcg == 0.0 else dcg / idcg,
        "first_relevant_rank": first_rank,
    }


def aggregate_metrics(metrics: list[dict[str, Any]]) -> dict[str, float]:
    if not metrics:
        return {"hit_at_k": 0.0, "recall_at_k": 0.0, "mrr": 0.0, "ndcg_at_k": 0.0}
    count = len(metrics)
    return {
        "hit_at_k": sum(1 for row in metrics if row["hit"]) / count,
        "recall_at_k": sum(row["recall"] for row in metrics) / count,
        "mrr": sum(row["mrr"] for row in metrics) / count,
        "ndcg_at_k": sum(row["ndcg"] for row in metrics) / count,
    }


def metric_delta(metrics: dict[str, float], baseline: dict[str, float]) -> dict[str, float]:
    return {key: metrics[key] - baseline[key] for key in baseline}


def rank_delta(
    per_query: list[dict[str, Any]], baseline_per_query: list[dict[str, Any]]
) -> dict[str, Any]:
    improved = 0
    worsened = 0
    unchanged = 0
    top1_changed = 0
    deltas = []
    for row, baseline in zip(per_query, baseline_per_query):
        missing = max(len(row["ranked_article_ids"]), len(baseline["ranked_article_ids"])) + 1
        rank = bounded_rank(row["metrics"]["first_relevant_rank"], missing)
        baseline_rank = bounded_rank(baseline["metrics"]["first_relevant_rank"], missing)
        delta = rank - baseline_rank
        deltas.append(delta)
        if delta < 0:
            improved += 1
        elif delta > 0:
            worsened += 1
        else:
            unchanged += 1
        if first_or_none(row["ranked_article_ids"]) != first_or_none(baseline["ranked_article_ids"]):
            top1_changed += 1
    return {
        "improved_queries": improved,
        "worsened_queries": worsened,
        "unchanged_queries": unchanged,
        "top1_changed_queries": top1_changed,
        "mean_first_relevant_rank_delta": sum(deltas) / len(deltas) if deltas else 0.0,
    }


def bounded_rank(rank: int | None, missing: int) -> int:
    return missing if rank is None else rank


def first_or_none(values: list[str]) -> str | None:
    return values[0] if values else None


def safe_ratio(value: int, baseline: int) -> float:
    return 0.0 if baseline == 0 else value / baseline


def latency_summary(values: list[float]) -> dict[str, float]:
    if not values:
        return {"p50_ms": 0.0, "p95_ms": 0.0, "max_ms": 0.0, "total_ms": 0.0}
    ordered = sorted(values)
    return {
        "p50_ms": percentile(ordered, 0.50),
        "p95_ms": percentile(ordered, 0.95),
        "max_ms": max(ordered),
        "total_ms": sum(values),
    }


def percentile(ordered_values: list[float], percentile_value: float) -> float:
    index = math.ceil((len(ordered_values) - 1) * percentile_value)
    return float(ordered_values[min(index, len(ordered_values) - 1)])


def chunked(values: list[str], chunk_size: int) -> list[list[str]]:
    return [values[index : index + chunk_size] for index in range(0, len(values), chunk_size)]


def write_json(path: Path, payload: dict[str, Any]) -> None:
    if path.parent and str(path.parent) != "":
        path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", encoding="utf-8") as handle:
        json.dump(payload, handle, indent=2)
        handle.write("\n")


def print_summary(report: dict[str, Any], top_k: int) -> None:
    print(
        "wrote quantization oracle report: "
        f"queries={report['query_count']} chunks={report['chunk_count']} top_k={top_k}"
    )
    for name, profile in report["profiles"].items():
        metrics = profile["metrics"]
        delta = profile["quality_delta_vs_float32"]
        ratio = profile["estimated_vector_bytes"]["chunk_ratio_vs_float32"]
        print(
            f"  {name}: recall@{top_k}={metrics['recall_at_k']:.3f} "
            f"hit@{top_k}={metrics['hit_at_k']:.3f} "
            f"mrr={metrics['mrr']:.3f} ndcg@{top_k}={metrics['ndcg_at_k']:.3f} "
            f"delta_mrr={delta['mrr']:+.4f} bytes={ratio:.2f}x"
        )


if __name__ == "__main__":
    raise SystemExit(main())
