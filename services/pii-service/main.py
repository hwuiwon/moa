"""FastAPI wrapper for the openai/privacy-filter token classifier."""

from __future__ import annotations

import os
from functools import lru_cache
from typing import Any

from fastapi import FastAPI, HTTPException
from pydantic import BaseModel
from transformers import pipeline


MODEL = os.getenv("MODEL", "openai/privacy-filter")
DEVICE = os.getenv("DEVICE", "cpu")

app = FastAPI(title="MOA PII Service", version="0.1.0")


class ClassifyRequest(BaseModel):
    """Request body for PII classification."""

    text: str
    return_spans: bool = True


class PiiSpan(BaseModel):
    """One normalized PII span returned to Rust clients."""

    start: int
    end: int
    category: str
    confidence: float


class ClassifyResponse(BaseModel):
    """Response body for PII classification."""

    spans: list[PiiSpan]
    abstained: bool = False
    model_version: str


@lru_cache(maxsize=1)
def classifier() -> Any:
    """Loads and memoizes the HuggingFace token-classification pipeline."""

    device = 0 if DEVICE == "cuda" else -1
    return pipeline(
        "token-classification",
        model=MODEL,
        aggregation_strategy="simple",
        device=device,
    )


@app.get("/healthz")
async def healthz() -> dict[str, str]:
    """Returns a lightweight health response."""

    return {"status": "ok", "model": MODEL, "device": DEVICE}


@app.post("/classify", response_model=ClassifyResponse)
async def classify(request: ClassifyRequest) -> ClassifyResponse:
    """Runs token classification and returns normalized spans."""

    try:
        raw_spans = classifier()(request.text)
    except Exception as exc:  # noqa: BLE001 - surface model runtime failures through HTTP.
        raise HTTPException(status_code=500, detail=str(exc)) from exc

    spans = [normalize_span(span) for span in raw_spans if should_keep_span(span)]
    return ClassifyResponse(
        spans=spans if request.return_spans else [],
        abstained=False,
        model_version=f"{MODEL}:v1.0",
    )


def should_keep_span(span: dict[str, Any]) -> bool:
    """Returns whether a HuggingFace span has usable offsets and labels."""

    return span.get("start") is not None and span.get("end") is not None and span_label(span)


def normalize_span(span: dict[str, Any]) -> PiiSpan:
    """Normalizes one HuggingFace token-classification span."""

    return PiiSpan(
        start=int(span["start"]),
        end=int(span["end"]),
        category=normalize_label(span_label(span)),
        confidence=float(span.get("score", 0.0)),
    )


def span_label(span: dict[str, Any]) -> str:
    """Extracts a model label from common HuggingFace response keys."""

    return str(span.get("entity_group") or span.get("entity") or span.get("label") or "")


def normalize_label(label: str) -> str:
    """Maps model BIOES labels into MOA's eight public categories."""

    normalized = (
        label.removeprefix("B-")
        .removeprefix("I-")
        .removeprefix("E-")
        .removeprefix("S-")
        .replace("-", "_")
        .replace(" ", "_")
        .upper()
    )
    aliases = {
        "NAME": "PERSON",
        "PRIVATE_PERSON": "PERSON",
        "PER": "PERSON",
        "PRIVATE_EMAIL": "EMAIL",
        "EMAIL_ADDRESS": "EMAIL",
        "PRIVATE_PHONE": "PHONE",
        "PHONE_NUMBER": "PHONE",
        "TELEPHONE": "PHONE",
        "PRIVATE_ADDRESS": "ADDRESS",
        "LOCATION": "ADDRESS",
        "STREET_ADDRESS": "ADDRESS",
        "SOCIAL_SECURITY_NUMBER": "SSN",
        "MEDICAL_RECORD_NUMBER": "MEDICAL_RECORD",
        "MRN": "MEDICAL_RECORD",
        "ACCOUNT_NUMBER": "FINANCIAL_ACCOUNT",
        "BANK_ACCOUNT": "FINANCIAL_ACCOUNT",
        "CREDIT_CARD": "FINANCIAL_ACCOUNT",
        "CARD_NUMBER": "FINANCIAL_ACCOUNT",
        "GOV_ID": "GOVERNMENT_ID",
        "PASSPORT": "GOVERNMENT_ID",
        "DRIVER_LICENSE": "GOVERNMENT_ID",
        "PRIVATE_URL": "URL",
        "PRIVATE_DATE": "DATE",
    }
    return aliases.get(normalized, normalized)
