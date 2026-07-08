# MOA 100-Persona Sweep Cases (2026-07-08 regeneration)

Case source for `.agents/skills/moa-100-session-sweep`. The original
2026-07-01 delegation-scheduler sweep report was lost in a local markdown
cleanup (it was never committed); this suite regenerates the 100 cases in
the runner's parse format and is committed so it cannot be lost again.
Runs seeded from this file start a NEW baseline; pre-2026-07-08 reports are
not comparable. Load-bearing request tokens preserved per
`project_planner_anchor_live_coverage`: ' reconcile ', ' summarize ',
' categorize '.

### S001 - Startup CFO - Scenario 1
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft a board-ready runway summary for Q3: cash on hand is $4.2M, monthly burn $310K, and we expect a $500K enterprise payment in August. Call out the runway with and without that payment.

### S002 - Startup CFO - Scenario 2
- Expected skills: finance-reporting
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: In parallel, delegate three independent workstreams: a revenue variance analysis vs plan for Q2, a vendor spend diligence pass on our top ten vendors, and a churn-adjusted forecast comparison. Then combine them into one board narrative.

### S003 - Finance Analyst - Scenario 3
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Please reconcile the June credit-card ledger against the exported expense report: flag duplicate charges, missing receipts, and anything over $2,000 without an approval note.

### S004 - Finance Analyst - Scenario 4
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize our AWS, GCP, and Snowflake spend trends over the last two quarters and recommend where to negotiate committed-use discounts.

### S005 - Controller - Scenario 5
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Build a month-end close plan for a five-person finance team: sequence the reconciliation steps, owners, and a two-day close target.

### S006 - Controller - Scenario 6
- Expected skills: finance-reporting
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Delegate two parallel analyses: one worker prepares a variance analysis of marketing spend vs budget, another prepares a headcount cost forecast through year end. Merge both into a single summary for the CFO.

### S007 - FP&A Lead - Scenario 7
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: We missed revenue plan by 12% last quarter. Draft a variance narrative for the board that separates pipeline slippage, churn, and pricing pressure, with one chart suggestion per section.

### S008 - FP&A Lead - Scenario 8
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Compare three forecast scenarios for next year (conservative, base, aggressive) given 8% MoM growth, 5% churn, and a hiring freeze in scenario one. Present as a table.

### S009 - Bookkeeper - Scenario 9
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Categorize these transactions into our chart of accounts: $1,250 Figma annual, $89 team lunch, $4,400 contractor invoice for landing-page work, $230 Delta ticket, and $12,000 Q3 office rent.

### S010 - Startup CEO - Scenario 10
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Give me a one-page cash summary I can paste into the investor update: burn, runway, top three cost drivers, and what changed since last month.

### S011 - Finance Ops - Scenario 11
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A vendor invoiced us twice for the same SOW milestone. Draft the dispute email and a short internal note on how to reconcile the duplicate in the AP ledger.

### S012 - FP&A Lead - Scenario 12
- Expected skills: finance-reporting
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Run these in parallel as separate workers: (1) a cohort revenue retention table description for 2025 signups, (2) a vendor spend categorization by department, (3) a runway sensitivity analysis at ±15% burn. Then synthesize the three into one memo.

### S013 - Startup CFO - Scenario 13
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`true`, cancel=`false`
- User request: Our Series B data room needs a revenue recognition summary. Explain how we should present annual prepaid contracts vs monthly plans, and list the schedules we need.

### S014 - Controller - Scenario 14
- Expected skills: finance-reporting
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Write the quarter-close checklist email to the team: bank reconciliation, accrual review, deferred revenue schedule, and flux analysis, each with an owner placeholder and due date.

### S015 - Support Lead - Scenario 15
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A customer on the $499/year plan was double-charged at renewal and is threatening a chargeback. Draft the apology email, the refund plan, and a note for the billing team.

### S016 - Support Lead - Scenario 16
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize our refund policy into five plain-language bullets a new support agent can quote directly in tickets.

### S017 - Support Agent - Scenario 17
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Customer bought a damaged espresso machine 40 days ago; our return window is 30 days but the damage claim has photos from day two. Recommend refund vs replacement and draft the reply.

### S018 - Support Agent - Scenario 18
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Categorize these five open billing tickets by refund eligibility and urgency: late-renewal dispute, duplicate charge, buyer's remorse day 3, failed delivery, and a chargeback already filed.

### S019 - CX Manager - Scenario 19
- Expected skills: refund-triage
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Delegate in parallel: one worker drafts a refund-decision tree for agents, a second drafts macros for the top four refund scenarios, a third summarizes last month's refund reasons from the notes I paste next. Combine into one enablement doc.

### S020 - CX Manager - Scenario 20
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A VIP customer wants an exception to the return window for a gift purchase. Write the exception approval note with conditions, and the customer reply.

### S021 - Support Lead - Scenario 21
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Our chargeback rate hit 1.2% last month. List the top likely causes for a subscription business and a remediation plan with owners.

### S022 - Billing Specialist - Scenario 22
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft the response to a bank's chargeback evidence request for a $1,900 annual subscription: what evidence we should attach and a cover summary.

### S023 - Support Agent - Scenario 23
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Customer claims they cancelled before renewal but our logs show the cancellation happened two hours after the charge. Draft a goodwill resolution and the internal policy note.

### S024 - CX Manager - Scenario 24
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Write a short SOP for partial refunds on annual plans: proration rules, approval thresholds, and the exact fields to record in the ticket.

### S025 - Support Lead - Scenario 25
- Expected skills: refund-triage
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`true`
- User request: Split this into two parallel worker tasks: draft the customer-facing apology for this weekend's failed deliveries, and separately prepare the make-good matrix (credit vs redelivery vs refund) by order value. Then give me both in one reply.

### S026 - Billing Specialist - Scenario 26
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A customer paid by wire for an enterprise plan and wants a refund to a different entity. Flag the compliance concerns and draft the finance handoff note.

### S027 - Support Agent - Scenario 27
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize this dispute thread for escalation in six lines: customer ordered two units, one arrived, courier says delivered, customer disputes, replacement is out of stock until next month.

### S028 - CX Manager - Scenario 28
- Expected skills: refund-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Create the weekly refund report template: volume, top reasons, average resolution time, chargebacks opened, and one narrative paragraph.

### S029 - SRE On-call - Scenario 29
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: We had a 40-minute checkout outage this morning caused by an expired TLS cert on the payments proxy. Draft the incident summary: impact, root cause, timeline, and three mitigations.

### S030 - SRE On-call - Scenario 30
- Expected skills: incident-triage
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Delegate three parallel workers: one drafts the customer status-page update for the ongoing API latency incident, one prepares the internal timeline from the notes I provide, one lists probable root causes for elevated p99 after a deploy. Combine into an incident packet.

### S031 - Ops Manager - Scenario 31
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Three cold-chain shipments arrived above temperature this week. Triage: likely causes, immediate containment, who owns the carrier escalation, and the customer comms plan.

### S032 - Engineering Manager - Scenario 32
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Write the postmortem skeleton for last night's database failover that dropped 2% of writes: sections, prompts for each, and the five whys starter.

### S033 - SRE On-call - Scenario 33
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Alert fatigue: we get ~120 pages a week and act on maybe ten. Propose an alert triage and pruning plan with a categorize-then-prune workflow for the top offenders.

### S034 - Ops Manager - Scenario 34
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize this ops spike for the exec channel in five bullets: order volume 3x from a promo, warehouse backlog 9 hours, two carriers missed pickup, support queue at 400.

### S035 - Engineering Manager - Scenario 35
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A customer reported data missing from their dashboard; we traced it to a silent ETL failure over the weekend. Draft the customer notification and the internal action-item list.

### S036 - SRE On-call - Scenario 36
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Create a severity matrix (SEV1-SEV4) for our delivery platform with concrete examples, response times, and who gets paged for each.

### S037 - Ops Manager - Scenario 37
- Expected skills: incident-triage
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`true`, cancel=`false`
- User request: Run two workers in parallel: one triages the warehouse scanner outage (impact, workaround, owner), the other drafts the shift-handoff status update. Merge into one operations bulletin.

### S038 - Engineering Manager - Scenario 38
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Our error budget for the month is nearly burned after two incidents. Recommend what to freeze, what to fast-track, and how to message the tradeoff to product.

### S039 - SRE On-call - Scenario 39
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft the status update sequence for a partial outage that is degrading but not down: initial, 30-minute update, and resolution templates.

### S040 - Ops Manager - Scenario 40
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A driver app bug sent 60 deliveries to a depot address. Triage the blast radius, the redelivery plan, and the customer apology template.

### S041 - Engineering Manager - Scenario 41
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Turn these raw notes into a crisp incident review doc: deploy at 14:02, feature flag mis-scoped, checkout errors 14:05-14:31, rollback 14:28, no data loss.

### S042 - SRE On-call - Scenario 42
- Expected skills: incident-triage
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: List the top eight leading indicators we should monitor to catch payment-provider degradation before customers do, each with a suggested threshold.

### S043 - CISO - Scenario 43
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A vendor wants read access to our production customer table for analytics. Draft the risk assessment: data classes exposed, alternatives, and the conditions under which we would allow it.

### S044 - CISO - Scenario 44
- Expected skills: security-review
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Delegate three parallel reviews: one worker assesses the SOC 2 evidence gaps from the list I share, one drafts the vendor security questionnaire for a new payroll provider, one summarizes our data-retention exceptions. Combine into a quarterly security memo.

### S045 - Security Engineer - Scenario 45
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Review this access pattern: contractors get owner-level access to the analytics workspace by default. Recommend the least-privilege redesign and the migration steps.

### S046 - Privacy Officer - Scenario 46
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: We want to store support-call transcripts for model training. Outline the privacy review: lawful basis, retention limits, redaction requirements, and opt-out handling.

### S047 - Security Engineer - Scenario 47
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: An auditor asked for evidence of quarterly access reviews. Draft the evidence-request response and a checklist to make the next review painless.

### S048 - CISO - Scenario 48
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize the security implications of enabling a browser extension for all employees that reads page content, and give a conditional-approval policy.

### S049 - Privacy Officer - Scenario 49
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`true`
- User request: A customer filed a deletion request but has an open invoice dispute. Explain what we must delete now, what we can retain, and draft the response.

### S050 - Security Engineer - Scenario 50
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Categorize these findings from the pentest report by severity and exploitability, and propose the fix order: exposed staging bucket, weak JWT expiry, verbose error pages, missing rate limit on login, and stale admin account.

### S051 - CISO - Scenario 51
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft a data-retention policy table for tickets, logs, analytics events, and backups: retention period, legal hold handling, and deletion mechanism for each.

### S052 - Privacy Officer - Scenario 52
- Expected skills: security-review
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Two parallel worker tasks: draft the DPIA outline for our new location-tracking feature, and separately review the marketing team's plan to upload customer emails to an ad platform. Return one combined recommendation.

### S053 - Security Engineer - Scenario 53
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Our on-call engineers share a single admin account for the payments console. Write the risk statement and a two-week remediation plan.

### S054 - CISO - Scenario 54
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A prospective enterprise customer sent a 40-question security questionnaire. Draft answers for the five hardest topics: encryption at rest, key rotation, subprocessor list, incident SLAs, and data residency.

### S055 - Privacy Officer - Scenario 55
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Review whether we can use production data in the staging environment if we pseudonymize emails and names, and list the controls required.

### S056 - Security Engineer - Scenario 56
- Expected skills: security-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Write the access-review SOP: scope, frequency, evidence capture, revocation deadlines, and the escalation path for unowned accounts.

### S057 - Product Manager - Scenario 57
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Break the Q4 checkout-redesign launch into a sequenced plan with milestones, owners by role, dependencies, and a go/no-go checklist two weeks before launch.

### S058 - Product Manager - Scenario 58
- Expected skills: project-planning
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`true`, cancel=`false`
- User request: Delegate three parallel planning workers: one drafts the engineering milestone plan for the mobile app rewrite, one drafts the marketing launch plan, one drafts the support-readiness plan. Merge into a single cross-functional roadmap.

### S059 - Operations Director - Scenario 59
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Plan our warehouse migration to the new facility over six weeks with zero order-processing downtime: phases, decision points, and rollback options.

### S060 - Engineering Manager - Scenario 60
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Sequence the Postgres 15 to 17 upgrade across twelve services: pre-checks, canary order, freeze windows, and the abort criteria.

### S061 - Head of People - Scenario 61
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft a hiring plan to grow support from six to fifteen people in two quarters: role mix, sourcing milestones, interviewer load, and onboarding waves.

### S062 - Product Manager - Scenario 62
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize this messy planning thread into a decision log: we chose usage-based pricing, deferred the enterprise tier, and moved the beta to November; capture owners and open questions.

### S063 - Founder - Scenario 63
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: I have four goals for next quarter and capacity for two: expand to Canada, launch referrals, rebuild onboarding, achieve SOC 2. Force-rank with rationale and a plan for the top two.

### S064 - Operations Director - Scenario 64
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Create the rollout plan for the new inventory system across three warehouses: pilot criteria, training, parallel-run period, and cutover checklist.

### S065 - Engineering Manager - Scenario 65
- Expected skills: project-planning
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Two workers in parallel: one plans the API deprecation timeline (comms, versioning, sunset dates), the other plans the internal migration off the old endpoints. Combine into one deprecation program plan.

### S066 - Head of People - Scenario 66
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Plan the offsite for 40 people in October: workstream owners, budget lines, agenda skeleton, and the decisions we need six weeks out.

### S067 - Product Manager - Scenario 67
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Turn this goal into milestones: reduce onboarding time from 14 days to 5 by Q1, covering product changes, docs, and CS process, with a measurable checkpoint each month.

### S068 - Founder - Scenario 68
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft a 30-60-90 plan for the incoming VP of Operations, focused on delivery reliability, vendor consolidation, and team structure.

### S069 - Operations Director - Scenario 69
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: We keep missing weekly ship goals by ~20%. Propose a planning-process fix: estimation, buffer policy, mid-week checkpoint, and the metric to track.

### S070 - Engineering Manager - Scenario 70
- Expected skills: project-planning
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Lay out the dependency map and sequencing for replacing our auth provider: data export, dual-run, cutover, and the customer-facing changes.

### S071 - Ops Lead - Scenario 71
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`true`, cancel=`false`
- User request: Review our ticket intake flow: everything lands in one inbox and agents self-assign. Propose a queue design with routing rules, SLAs by category, and a categorize step at intake.

### S072 - Ops Lead - Scenario 72
- Expected skills: workflow-review
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Delegate in parallel: one worker maps the current order-to-fulfillment handoffs from my notes, another drafts the improved SOP with approval gates removed where safe. Return a before/after comparison.

### S073 - COO - Scenario 73
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Our expense approvals take nine days on average. Diagnose the likely bottlenecks in a three-step approval chain and propose a redesign with a 48-hour target.

### S074 - Support Manager - Scenario 74
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Write the SOP for escalating bugs from support to engineering: what evidence to attach, severity mapping, response-time expectations, and the feedback loop.

### S075 - Ops Lead - Scenario 75
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize the differences between our two regional fulfillment SOPs and recommend the unified version, keeping the stricter QA step.

### S076 - COO - Scenario 76
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: We have eleven weekly status meetings. Categorize them by decision vs broadcast vs coordination, and propose which to kill, merge, or convert to async.

### S077 - Support Manager - Scenario 77
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`true`
- User request: Design the intake form for the new professional-services request queue: required fields, auto-routing rules, and the rejection criteria with canned responses.

### S078 - Ops Lead - Scenario 78
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Our returns process has six handoffs and takes 12 days. Map the ideal five-day version: steps, owners, systems touched, and the metrics to prove it works.

### S079 - COO - Scenario 79
- Expected skills: workflow-review
- Expected worker delegation: `true`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Two parallel workers: one reviews the vendor onboarding workflow for redundant approvals, the other drafts the metrics dashboard spec (cycle time, first-pass yield, queue age). Combine recommendations.

### S080 - Support Manager - Scenario 80
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft the QA rubric for support replies: accuracy, tone, policy compliance, and resolution completeness, scored 1-5 with examples.

### S081 - Ops Lead - Scenario 81
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Our warehouse pick-pack error rate doubled after the layout change. Review the likely process causes and design the checkpoint that catches errors before shipping.

### S082 - COO - Scenario 82
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Propose a weekly business review format: the six metrics on page one, owner commentary rules, and the escalation trigger for off-track items.

### S083 - Support Manager - Scenario 83
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Turn this tribal knowledge into an SOP: refunds over $500 need lead approval, fraud flags go to trust, VIPs get callbacks within two hours, everything logged in the tracker.

### S084 - Ops Lead - Scenario 84
- Expected skills: workflow-review
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`true`, cancel=`false`
- User request: Review our on-call handoff process between shifts and draft the handoff template: open incidents, pending decisions, watch items, and links.

### S085 - Executive Assistant - Scenario 85
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Remember these preferences for future sessions: my exec prefers morning flights before 9am, aisle seats, Marriott properties, and no meetings on Friday afternoons. Confirm what you stored.

### S086 - Executive Assistant - Scenario 86
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: What travel preferences do you have stored for my exec? List them and flag anything that looks stale or sensitive.

### S087 - Sales Rep - Scenario 87
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Remember that Acme's procurement contact is Dana Wu, their fiscal year ends in January, and they require security review before any pilot. Also tell me what you should NOT store from a deal thread.

### S088 - Recruiter - Scenario 88
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: I want to save candidate interview notes in our sessions. Which parts are safe to remember, which are sensitive personal data we should not retain, and how long should notes live?

### S089 - Sales Rep - Scenario 89
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Retrieve everything you remember about my Q3 pipeline preferences and summarize it in four bullets, then remove anything about the lost Initech deal.

### S090 - Customer Success Manager - Scenario 90
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Remember that Globex prefers monthly check-ins on Tuesdays, their exec sponsor is the CFO, and renewals route through procurement. Confirm storage and retention.

### S091 - Executive Assistant - Scenario 91
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: A teammate pasted a passport number into our chat last week. Should that be stored, and what should we do about it now? Give the privacy-safe cleanup steps.

### S092 - Recruiter - Scenario 92
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`true`
- User request: Store my sourcing preferences: prioritize backend engineers with Rust or Go, remote-first, and skip agencies. What retention policy applies to preferences like these?

### S093 - Customer Success Manager - Scenario 93
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Which of these should you remember long-term vs discard after this session: customer's NPS verbatim, their AWS account ID, the renewal date, and the champion's personal cell number?

### S094 - Sales Rep - Scenario 94
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Remember my demo script preference: start with the reporting module, skip the admin console, always end with pricing ranges. Then retrieve and confirm it back to me.

### S095 - Executive Assistant - Scenario 95
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Summarize what you currently remember about my scheduling preferences and mark each item keep, update, or delete.

### S096 - Recruiter - Scenario 96
- Expected skills: memory-privacy-check
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`true`, cancel=`false`
- User request: We're doing a privacy cleanup: list the categories of candidate data that should never persist in session memory and why, in a table I can share.

### S097 - Office Manager - Scenario 97
- Expected skills: none
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Draft a friendly all-hands announcement: the office moves to the 4th floor next month, badge access changes Friday, and plants need adopting.

### S098 - Founder - Scenario 98
- Expected skills: none
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Give me three options for a two-day team offsite within driving distance of Austin, with rough per-person costs and one signature activity each.

### S099 - Marketing Lead - Scenario 99
- Expected skills: none
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Rewrite this product blurb to be half the length and twice as concrete: 'Our platform leverages synergies to holistically empower modern teams to do their best work.'

### S100 - Office Manager - Scenario 100
- Expected skills: none
- Expected worker delegation: `false`
- Interrupt/cancel path: interrupt=`false`, cancel=`false`
- User request: Create a simple rotating snack-and-coffee duty roster for eight people over eight weeks, with a swap rule.
