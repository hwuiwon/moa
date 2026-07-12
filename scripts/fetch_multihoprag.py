#!/usr/bin/env python3
"""Fetch MultiHop-RAG (yixuantt, COLM 2024) and convert it to the WixQA lane format.

MultiHop-RAG is the external relationship-based retrieval benchmark from the
2026-07-11 RAG accuracy plan (docs/engineering-discipline/plans): 2,556 news
queries whose gold evidence spans 2-4 of 609 articles. This script downloads
the two raw JSON files from Hugging Face (idempotent: skipped when present)
and converts them into the article/question JSONL shape consumed by
`cargo xtask wixqa-rag-eval --dataset multihoprag`.

Null queries (unanswerable by construction) are excluded from the retrieval
question set: the WixQA harness scores evidence recall and has no abstention
protocol. Their count is reported so an abstention slice can be added later.

Usage: python3 scripts/fetch_multihoprag.py [--data-dir .moa/wixqa/raw]
"""

import argparse
import hashlib
import json
import pathlib
import sys
import urllib.request

HF_BASE = "https://huggingface.co/datasets/yixuantt/MultiHopRAG/resolve/main"
RAW_FILES = ["corpus.json", "MultiHopRAG.json"]


def article_id(url: str) -> str:
    return "mh-" + hashlib.sha1(url.encode()).hexdigest()[:12]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--data-dir", default=".moa/wixqa/raw")
    args = parser.parse_args()
    out_dir = pathlib.Path(args.data_dir) / "multihoprag"
    out_dir.mkdir(parents=True, exist_ok=True)

    for name in RAW_FILES:
        path = out_dir / name
        if path.exists() and path.stat().st_size > 1000:
            continue
        print(f"downloading {name} ...")
        urllib.request.urlretrieve(f"{HF_BASE}/{name}", path)

    corpus = json.loads((out_dir / "corpus.json").read_text())
    queries = json.loads((out_dir / "MultiHopRAG.json").read_text())

    with open(out_dir / "corpus.jsonl", "w") as fh:
        for article in corpus:
            fh.write(
                json.dumps(
                    {
                        "id": article_id(article["url"]),
                        "url": article["url"],
                        "contents": article["body"],
                        "title": article["title"],
                        "article_type": article.get("category") or "news",
                    }
                )
                + "\n"
            )

    known_ids = {article_id(article["url"]) for article in corpus}
    kept, dropped_null, dropped_missing = 0, 0, 0
    with open(out_dir / "questions.jsonl", "w") as fh:
        for query in queries:
            if query["question_type"] == "null_query":
                dropped_null += 1
                continue
            ids = sorted(
                {article_id(evidence["url"]) for evidence in query["evidence_list"]}
            )
            if not ids or any(article_id not in known_ids for article_id in ids):
                dropped_missing += 1
                continue
            fh.write(
                json.dumps(
                    {
                        "question": query["query"],
                        "article_ids": ids,
                        "question_type": query["question_type"],
                    }
                )
                + "\n"
            )
            kept += 1

    print(
        f"wrote {out_dir}/corpus.jsonl ({len(corpus)} articles) and "
        f"questions.jsonl ({kept} questions; dropped {dropped_null} null, "
        f"{dropped_missing} with missing evidence articles)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
