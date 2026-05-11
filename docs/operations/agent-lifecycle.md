# Agent lifecycle

Agents are first-class MOA principals. They have FGA subject identity
(`agent:<uuid>`) and can receive permissions independently from the user who
operates them.

## Templates and instances

An agent template is a reusable definition: name, description, instructions,
and allowed tool names. An agent is an instance of a template with its own UUID,
operator, API keys, and delegation grants.

## Permission tuples

Template creation writes:

```text
tenant:<tenant> tenant agent_template:<template>
user:<creator> creator agent_template:<template>
```

Agent registration writes:

```text
tenant:<tenant> tenant agent:<agent>
user:<operator> operator agent:<agent>
```

Delegation uses the OpenFGA object shape from `schema_v1.fga`:

```text
user:<user> can_act_as agent:<agent>
```

That tuple lets `require_authz_with_delegation` verify that an agent may act on
behalf of a user. It does not grant resource access by itself.

## Deactivation

`Agents::deactivate` marks the row deactivated, enqueues deletes for the
agent's `operator`, `tenant`, and `can_act_as` tuples, and revokes active API
keys owned by the agent with reason `agent_deactivation_cascade`.

Deactivation does not remove direct resource tuples such as
`agent:<agent> editor workspace:<workspace>`, because listing every resource
tuple for an agent is an expensive full FGA read. Operators should prefer
delegation grants for P1.8 and remove direct resource grants manually when used.
