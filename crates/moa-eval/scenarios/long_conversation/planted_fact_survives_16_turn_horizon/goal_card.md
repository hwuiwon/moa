# Planted Fact Survives A 16-Turn Horizon

The user states one segment-critical operational fact on turn 1, then holds a
long unrelated conversation. Fifteen turns later the user asks the agent to
recall the fact. The agent must answer with the original fact intact.

This scenario pins the memory/context-degradation failure mode: a fact stated
early must remain retrievable from the durable session log across a long
horizon, measured by the score card's planted-fact recall.
