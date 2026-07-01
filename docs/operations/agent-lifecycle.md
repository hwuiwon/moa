# Agent lifecycle

Agents are first-class MOA principals. They have FGA subject identity
(`agent:<uuid>`) and can receive permissions independently from the user who
operates them.

## Agents and specialization

An agent is a first-class principal with its own UUID, operator, API keys, and
delegation grants. It is not instantiated from a template. Task specialization
comes from the compiled context, ranked Agent Skills, tool schemas, memory, and
bounded workers.

There is no durable agent-template taxonomy for routing. Runtime routing and
task specialization are handled by skills, context, tools, memory, and bounded
workers.

## Permission tuples

Agent registration writes:

```text
tenant:<tenant> tenant agent:<agent>
user:<operator> operator agent:<agent>
```

Delegation uses the OpenFGA object shape from `schema_v1.json`:

```text
user:<user> can_act_as agent:<agent>
```

That tuple lets `require_authz_with_delegation` verify that an agent may act on
behalf of a user. It does not grant resource access by itself.

## Deactivation

`Agents::deactivate` marks the row deactivated, enqueues deletes for the
agent's `operator`, `tenant`, and `can_act_as` tuples, and revokes active API
keys owned by the agent with reason `agent_deactivation_cascade`.

Deactivation does not remove arbitrary direct resource tuples, because listing
every resource tuple for an agent is an expensive full FGA read. Operators
should prefer delegation grants and remove direct resource grants manually when
used.
