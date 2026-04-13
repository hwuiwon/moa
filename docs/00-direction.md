# 00 — Direction

_Product identity, philosophy, target users._

---

## What MOA is

MOA is a **cloud-first, Rust-based, general-purpose agent platform** built on the **many-brains, many-hands** pattern. Users interact through **messaging apps** (Telegram, Slack, Discord). The system **learns from usage** via a file-backed wiki memory. A **TUI** provides a zero-setup local on-ramp.

> MOA is a persistent, learning, general-purpose agent that lives in the cloud, reaches you through messaging, and gets better the longer it runs.

## What MOA provides

- A **durable session layer** — append-only event log that outlives any brain or hand
- **Stateless brains** — harness loops that crash-recover from the session log
- **Pluggable hands** — execution environments behind `execute(name, input) → output`
- A **learning loop** — file-wiki memory that compounds with every session
- **Messaging-first UX** — Telegram, Slack, Discord as primary interfaces
- A **TUI on-ramp** — full local experience with zero cloud dependencies

## What MOA is not

- A chatbot wrapper around one LLM
- A coding-only agent (though coding is a first-class hand)
- Local-only — local is the easy on-ramp, cloud is the production target
- Opinionated about which LLM powers the brain — model/provider flexibility is core

## Design values

- **Inspectability over magic.** Sessions, context compilation, tool calls, memory — all observable.
- **Reversible collaboration.** Inspect → approve → checkpoint → revert. The human stays in control.
- **Model/provider flexibility.** OpenAI, Anthropic, and Google Gemini at launch. No vendor lock-in.
- **Complexity must justify itself.** If it doesn't improve daily use, it doesn't ship in the default path.
- **Daily-driver UX beats impressive demos.** Predictable, low-friction, no cognitive fatigue.
- **No "impressive demo" features that degrade the daily path.**

## Target users

General. MOA handles any request a user can describe — coding, research, scheduling, data analysis, creative work, system administration. The architecture does not privilege one task type.

Primary persona: a professional who wants an always-available agent that remembers their projects, preferences, and past work — accessed through the messaging app they already use, with the option to drop into a terminal for hands-on work.

## Competitive positioning

- **vs OpenClaw**: MOA competes on architectural soundness (brain/hand isolation, credential separation) and learning (wiki compilation, skill distillation). Not on breadth of integrations.
- **vs Hermes Agent**: MOA competes on brain/hand decoupling (Hermes runs everything in one process), session durability (Hermes lacks formal crash recovery), and Rust performance. MOA learns from Hermes's closed learning loop design.
- **vs Anthropic Managed Agents**: MOA is open, self-hosted, model-agnostic, and has a self-improving memory system.

## Source of truth hierarchy

When resolving ambiguity during implementation:

1. This spec (individual section documents)
2. Current repo code and tests
3. Older planning docs (background only)
