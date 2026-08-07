# Literature Map — MOA Related Work

Organized by methodology/theme. Every cite key below corresponds to a verified entry in
`paper/references.bib` (verified 2026-08-07 via arXiv API + Semantic Scholar batch lookup,
and DBLP/Crossref/DOI content negotiation for non-arXiv papers).

MOA in one line, for positioning: a production multi-tenant enterprise agent platform with a
regression-gated skill learning loop — deterministic composite scoring of task segments, LLM
distillation into Agent-Skills-style SKILL.md documents, a label-free regression oracle built
from "grounded facts" (spans in the final response corroborated word-boundary-exactly in
successful tool outputs), held-out sibling suites from other sessions of the same recurring
task, human review before promotion, a post-promotion regression monitor with rollback
proposals, and a PII sanitization gate that makes raw transcripts unrepresentable in the
learning path.

---

## Theme 1: Agent skill/experience learning and skill libraries

This line of work has agents accumulate reusable procedural knowledge from their own
trajectories: Voyager grows an ever-expanding library of executable code skills in Minecraft,
validated by in-game execution [wang2023voyager]; Agent Workflow Memory induces reusable
workflows from past web-navigation trajectories and injects them into memory
[wang2024awm]; ExpeL distills cross-task insights and success exemplars from experience
pools [zhao2023expel]; AutoManual writes and updates environment "manuals" of rules through
interactive learning [chen2024automanual]; SkillWeaver has web agents synthesize, practice,
and hone API-like skills [zheng2025skillweaver]; Cradle acquires computer-control skills
through a self-improvement loop over screenshots and keyboard/mouse actions [tan2024cradle];
and CLIN maintains a continually updated causal-abstraction memory to improve across trials
[majumder2023clin]. MOA shares the distill-experience-into-reusable-artifacts premise (in the
SKILL.md format popularized by Anthropic's Agent Skills [anthropic2025skills]) but differs on
the admission side: none of these systems require a candidate skill to pass a deterministic,
label-free regression suite plus held-out sibling suites from other sessions before promotion,
none interpose human review and a post-promotion regression monitor with rollback, and none
operate under multi-tenant isolation with a PII sanitization gate that makes raw transcripts
unrepresentable in the learning path — their validation is either execution success in a
sandboxed environment, LLM self-judgment, or downstream task reward.

## Theme 2: Self-improvement of LLM agents and its safety

Self-improvement methods let models or agents iterate on their own outputs or their own
machinery: Self-Refine loops generator and self-feedback [madaan2023selfrefine], Reflexion
converts environment feedback into verbal self-reflections stored in episodic memory
[shinn2023reflexion], and STaR bootstraps training data from the model's own rationales
[zelikman2022star]; at the extreme, the Darwin Gödel Machine evolves populations of coding
agents that rewrite their own code, gated by benchmark scores [zhang2025dgm], and surveys now
map this space of self-evolving models and agents [tao2024selfevolution, gao2025selfevolving].
The safety literature warns that such loops are fragile: training on self-generated data
degrades models (model collapse) [shumailov2023curse], and optimizing proxy objectives
invites reward hacking [pan2022rewardmisspec]. MOA is a self-improving agent system that takes
these failure modes as design constraints rather than afterthoughts: improvement is confined
to auditable skill documents (never weights, never raw trajectories), the gating signal is a
deterministic corroboration oracle that is hard to game because it checks exact word-boundary
agreement between response spans and successful tool outputs, generalization is enforced by
held-out sibling suites, and promotion is reversible via a monitored rollback path with a
human in the loop.

## Theme 3: Verification and evaluation of agents without labels

Because labeled outcomes are scarce, prior work verifies agent/LLM outputs with (a) LLM
judges, whose biases and inconsistencies are well documented [zheng2023llmjudge,
wang2023fairevaluators]; (b) execution- or agreement-based proxies, e.g. generated unit tests
with dual execution agreement [chen2022codet] and self-consistency voting
[wang2022selfconsistency]; and (c) evidence-grounding/attribution checks, from the AIS
framework for attribution [rashkin2023ais] and attributed QA [bohnet2022attributedqa] to
citation-quality evaluation [gao2023alce], post-hoc attribution and revision [gao2023rarr],
and sampling-based hallucination detection [manakul2023selfcheckgpt]. MOA's grounded-facts
oracle belongs to family (c) but repurposes it: instead of measuring attribution quality of
free text against retrieved documents (usually with NLI models or LLM judges), it extracts
high-precision spans (numbers, IDs, URLs, paths, quoted spans) from the final response and
requires word-boundary-exact corroboration in the session's own successful tool outputs,
yielding a deterministic, reproducible pass/fail oracle that needs no labels and no LLM judge
— and it uses this oracle not to score single responses but to auto-generate regression
suites that gate skill promotion.

## Theme 4: Regression testing and behavioral testing for ML/LLM systems

Behavioral testing treats models like software under test: CheckList generates capability-
directed test suites for NLP models [ribeiro2020checklist], and the ML Test Score prescribes
a rubric of tests and monitoring for production ML pipelines [breck2017mltestscore]. A
parallel thread studies regressions caused by model updates — negative flips and
positive-congruent training in vision [yan2021pctraining], update regression in structured
prediction NLP [cai2022updateregression], and drift in the behavior of deployed LLM APIs
over time [chen2023chatgptbehavior]. MOA operationalizes this agenda for learned agent
skills: every candidate skill ships with a deterministically generated regression suite whose
test cases and oracle are derived from real production sessions rather than hand-written
templates, sibling suites from other sessions of the same recurring task serve as the
held-out behavioral battery, and the post-promotion monitor that compares resolution rates
and files rollback proposals is precisely a continuous negative-flip detector at the level of
skills instead of model weights.

## Theme 5: Safe deployment practices and ML production reliability

The MLOps literature documents why deployed ML needs guardrails: hidden technical debt and
feedback loops [sculley2015hidden], a taxonomy of deployment failures across the lifecycle
[paleyes2022challenges], the centrality of monitoring, staged rollout, and rollback in
practitioners' workflows [shankar2022operationalizing], and organizational practices for
integrating ML into software engineering [amershi2019se4ml]. These works describe canary/
shadow deployment and rollback for models and pipelines; MOA transplants that discipline to
in-context skill artifacts in a continuously learning agent platform: promotion gates play
the role of pre-deployment tests, sibling suites act as shadow evaluation on traffic the
skill was not distilled from, the regression monitor is the canary metric (resolution-rate
comparison), and rollback proposals close the loop — with the added, unusual constraint that
the PII sanitization gate structurally prevents raw production transcripts from ever entering
the promoted artifact.

## Theme 6: Held-out evaluation, overfitting, and contamination

Repeated evaluation against a fixed benchmark corrupts the signal: adaptive leaderboard
overfitting has formal treatments and defenses [blum2015ladder], new test sets reveal
accuracy drops consistent with test-set adaptation [recht2019imagenet], and for LLMs,
train-test contamination [sainz2023contamination] and task contamination
[li2023taskcontamination] inflate reported few-shot ability; for agents specifically,
benchmark shortcuts and overfitting motivate calls for held-out, cost-controlled evaluation
[kapoor2024aiagents]. MOA faces the same threat internally — a skill distilled from a session
would trivially pass a regression suite generated from that same session — and answers it
architecturally: sibling suites accumulated from *other* sessions of the same recurring task
are the held-out set, so a candidate skill must generalize across sessions (different users,
parameters, and tool outputs) before promotion, and the suites keep growing as new sessions
arrive, which resists the static-benchmark saturation failure mode.

---

## Five closest competitors and how MOA differs

1. **SkillWeaver [zheng2025skillweaver]** — closest in spirit: web agents autonomously
   synthesize skills, then "hone" them via generated practice tasks. Differences: SkillWeaver's
   verification is LLM-driven testing/debugging of API wrappers in a benchmark environment;
   MOA's gate is a deterministic grounded-facts oracle over production sessions (no LLM judge,
   no labels), plus held-out sibling suites, human review, multi-tenant/PII constraints, and
   post-promotion monitoring with rollback — none of which SkillWeaver has.

2. **Agent Workflow Memory [wang2024awm]** — induces reusable workflows from past
   trajectories and injects them into agent memory. Differences: AWM admits workflows without
   any regression gate (induction quality is validated only by end-task benchmark gains),
   stores trajectory-derived content directly (no sanitization boundary), and has no
   promotion/rollback lifecycle; MOA treats each workflow-like skill as a governed release
   artifact that must pass generated and held-out suites before exposure to tenants.

3. **Voyager [wang2023voyager]** — the canonical skill library, with skills verified by
   execution success in Minecraft. Differences: Voyager's oracle is environment reward in a
   single-user game where trial-and-error is free; MOA operates where re-execution against
   live enterprise systems is unsafe, so it replaces execution-replay with a corroboration
   oracle over recorded successful tool outputs and adds cross-session held-out evaluation,
   which Voyager does not need or have.

4. **ExpeL [zhao2023expel]** — distills insights and exemplars from experience pools for
   in-context reuse. Differences: ExpeL's insights are unaudited natural-language rules whose
   only validation is downstream benchmark accuracy, and its exemplars are raw trajectories;
   MOA's distilled skills are regression-tested individually before promotion, and the PII
   gate makes raw-transcript exemplars unrepresentable — the learning path carries only
   sanitized, verified skill documents.

5. **Darwin Gödel Machine [zhang2025dgm]** — self-improving agents gated by empirical
   benchmark validation, the strongest prior example of "gate self-modification with tests."
   Differences: DGM modifies agent *code* and gates on public coding benchmarks (subject to
   benchmark overfitting and objective hacking, which the paper itself observes); MOA modifies
   only declarative skill documents, generates its gates from the system's own production
   traffic with a hard-to-game exact-corroboration oracle, holds out sibling sessions to
   measure generalization, and wraps promotion in human review plus a rollback-capable
   production monitor.

(Also near: AutoManual [chen2024automanual], whose online rule system with error-driven
updates resembles skill curation but likewise lacks a label-free regression gate, held-out
suites, and a deployment lifecycle.)

---

## Verification notes

- All 38 bibliography entries verified in at least two independent sources; none required
  the [CITATION NEEDED] marker.
- `anthropic2025skills` is a vendor announcement (web page, verified live 2026-08-07), cited
  as @misc — it defines the SKILL.md format MOA adopts, but is not a peer-reviewed paper.
- Venue notes in the .bib reflect Semantic Scholar's recorded publication venue; years are
  the arXiv first-posted year. If the paper template requires official proceedings years
  (e.g., ExpeL appeared at AAAI 2024, AWM at ICML 2025), adjust the year/venue fields at
  camera-ready time against the proceedings.
