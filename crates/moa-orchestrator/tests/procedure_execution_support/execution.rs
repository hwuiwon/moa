// Deterministic procedure-node source fixtures.

fn deterministic_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: deterministic-routing
  description: Deterministic branch procedure for execution projection tests.
  tags:
    - test
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
      input_schema:
        type: object
        properties:
          decision:
            type: string
      nodes:
        - id: start
          kind: start
          ui:
            x: 80
            y: 120
        - id: route
          kind: condition
          ui:
            x: 280
            y: 120
        - id: approved
          kind: end
          input:
            route: approved
          ui:
            x: 520
            y: 80
        - id: rejected
          kind: end
          input:
            route: rejected
          ui:
            x: 520
            y: 160
      edges:
        - id: start-route
          from: start
          to: route
        - id: route-approved
          from: route
          to: approved
          when:
            type: equals
            left: input.decision
            right: approved
        - id: route-rejected
          from: route
          to: rejected
      ui:
        layout: dagre
"#
}

fn parallel_join_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: parallel-join-procedure
  description: Procedure that fans out two independent tool nodes and joins them.
  tags:
    - test
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
      nodes:
        - id: start
          kind: start
          ui:
            x: 80
            y: 160
        - id: fanout
          kind: parallel
          ui:
            x: 240
            y: 160
        - id: left
          kind: tool
          tool_refs:
            - tool://file_search
          input:
            pattern: "*"
          ui:
            x: 420
            y: 80
        - id: right
          kind: tool
          tool_refs:
            - tool://file_search
          input:
            pattern: "*"
          ui:
            x: 420
            y: 240
        - id: join
          kind: join
          ui:
            x: 620
            y: 160
        - id: done
          kind: end
          ui:
            x: 780
            y: 160
      edges:
        - id: start-fanout
          from: start
          to: fanout
        - id: fanout-left
          from: fanout
          to: left
        - id: fanout-right
          from: fanout
          to: right
        - id: left-join
          from: left
          to: join
        - id: right-join
          from: right
          to: join
        - id: join-done
          from: join
          to: done
      ui:
        layout: dagre
"#
}

fn loop_guard_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: loop-guard-procedure
  description: Procedure with an explicit conditional back-edge for loop guard coverage.
  tags:
    - test
status: draft
definition:
  type: skill
  spec:
    instructions:
      path: SKILL.md
    procedure:
      nodes:
        - id: start
          kind: start
          ui:
            x: 80
            y: 120
        - id: retry
          kind: condition
          ui:
            x: 280
            y: 120
        - id: done
          kind: end
          ui:
            x: 520
            y: 120
      edges:
        - id: start-retry
          from: start
          to: retry
        - id: retry-loop
          from: retry
          to: retry
          when:
            type: exists
            path: input.retry
        - id: retry-done
          from: retry
          to: done
      ui:
        layout: dagre
"#
}
