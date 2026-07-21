# moa-messaging

Messaging channel adapters and rendering helpers: the Slack Socket Mode
adapter, Postmark email and Twilio SMS connectors, plus channel-neutral
delivery, rate-limit pacing, and action-review rendering shared across
channels.

## Structure

- `action_review` — unified action-review rendering across channel
  adapters.
- `delivery` — channel-neutral delivery helpers for account and
  contact-facing messages.
- `postmark` — Postmark email notification connector.
- `rate_limit` — rate-limit retry and per-channel send pacing for messaging
  adapters.
- `renderer` — shared rendering helpers for messaging channel adapters.
- `slack` — Slack channel adapter built on `slack-morphism` Socket Mode.
- `twilio` — Twilio SMS notification connector.

## Features

All features are enabled by default.

- `slack` — Slack adapter and renderer (pulls in `slack-morphism`).
- `postmark` — Postmark email connector (pulls in `reqwest`).
- `twilio` — Twilio SMS connector (pulls in `reqwest`).
