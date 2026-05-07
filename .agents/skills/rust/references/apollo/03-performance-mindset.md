# Chapter 03 Notes: Performance Mindset

## Core Rule

- Do not guess. Measure.

## Practical Guidance

- Benchmark or profile in release mode when performance claims matter.
- Avoid redundant clones, especially in loops and iterator chains.
- Be conscious of large enum variants, large stack allocations, and unnecessary intermediate collections.
- Prefer iterator pipelines when they stay readable and avoid extra allocation.

## MOA Translation

- The brain pipeline, event replay, tool routing, and provider adapters are the places where accidental copies and needless collections matter most.
- Do not introduce `Box<dyn Trait>` or heap allocation patterns as "performance fixes" without evidence.
- If a path is latency-sensitive, inspect existing telemetry and tracing first before changing code shape.

## Good Triggers For This Note

- The task mentions slowness, hot paths, memory growth, or throughput.
- You see repeated clones of events, messages, or tool outputs.
- You are considering boxing large enum variants or changing iterator versus collection behavior.
