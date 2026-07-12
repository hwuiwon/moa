#!/usr/bin/env python3
"""Fetch FinanceBench (PatronusAI) and convert it to the WixQA lane format.

FinanceBench is an external financial-domain retrieval benchmark: 150
open-source questions over SEC filings (10-K/10-Q/8-K/earnings) from ~80+
companies. This script downloads the open-source subset from Hugging Face
(idempotent: skipped when present) and converts it into the article/question
JSONL shape consumed by `cargo xtask wixqa-rag-eval --dataset financebench`.
Note: the Hugging Face repo hosts the file as `financebench_merged.jsonl`
(every row carries `dataset_subset_label == "OPEN_SOURCE"`); the same data
lives at `data/financebench_open_source.jsonl` in the GitHub repo.

Corpus construction (honest limitation): the raw dataset ships only gold
evidence snippets (`evidence[].evidence_text`), not full filing text — the
full documents are PDFs that need a parsing pipeline. Each corpus "article"
is therefore synthetic: one per distinct `doc_name`, whose contents
concatenate every evidence_text snippet attributed to that document across
the whole dataset. Each question's gold article is its `doc_name` article,
and the distractors are the other documents' evidence snippets. This lane
measures evidence-snippet retrieval (can we route a question to the right
filing's evidence?), NOT full-10-K retrieval. Follow-up: build a
full-document corpus once the PDF parsing pipeline exists.

Questions whose document has no non-empty evidence text are skipped and
reported.

Usage: python3 scripts/fetch_financebench.py [--data-dir .moa/wixqa/raw]
"""

import argparse
import hashlib
import json
import pathlib
import sys
import urllib.request

HF_URL = (
    "https://huggingface.co/datasets/PatronusAI/financebench/resolve/main/"
    "financebench_merged.jsonl"
)
RAW_NAME = "financebench_merged.jsonl"


def article_id(doc_name: str) -> str:
    return "fb-" + hashlib.sha1(doc_name.encode()).hexdigest()[:12]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--data-dir", default=".moa/wixqa/raw")
    args = parser.parse_args()
    out_dir = pathlib.Path(args.data_dir) / "financebench"
    out_dir.mkdir(parents=True, exist_ok=True)

    raw_path = out_dir / RAW_NAME
    if not (raw_path.exists() and raw_path.stat().st_size > 1000):
        print(f"downloading {RAW_NAME} ...")
        urllib.request.urlretrieve(HF_URL, raw_path)

    rows = [json.loads(line) for line in raw_path.read_text().splitlines() if line.strip()]
    rows = [row for row in rows if row.get("dataset_subset_label") == "OPEN_SOURCE"]

    # One synthetic article per doc_name: concatenated deduped evidence snippets.
    doc_snippets: dict[str, list[str]] = {}
    for row in rows:
        snippets = doc_snippets.setdefault(row["doc_name"], [])
        for evidence in row.get("evidence") or []:
            text = (evidence.get("evidence_text") or "").strip()
            if text and text not in snippets:
                snippets.append(text)

    corpus_docs = {name: parts for name, parts in doc_snippets.items() if parts}
    with open(out_dir / "corpus.jsonl", "w") as fh:
        for doc_name in sorted(corpus_docs):
            fh.write(
                json.dumps(
                    {
                        "id": article_id(doc_name),
                        "url": doc_name,
                        "contents": "\n\n".join(corpus_docs[doc_name]),
                        "title": doc_name,
                        "article_type": "sec_filing",
                    }
                )
                + "\n"
            )

    kept, dropped_no_evidence = 0, 0
    with open(out_dir / "questions.jsonl", "w") as fh:
        for row in rows:
            if row["doc_name"] not in corpus_docs:
                dropped_no_evidence += 1
                continue
            fh.write(
                json.dumps(
                    {
                        "question": row["question"],
                        "article_ids": [article_id(row["doc_name"])],
                        "question_type": row["question_type"],
                    }
                )
                + "\n"
            )
            kept += 1

    print(
        f"wrote {out_dir}/corpus.jsonl ({len(corpus_docs)} articles) and "
        f"questions.jsonl ({kept} questions; dropped {dropped_no_evidence} "
        f"with no non-empty evidence text)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
