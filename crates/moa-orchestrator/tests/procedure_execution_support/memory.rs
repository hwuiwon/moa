// Memory procedure-node source fixtures.

use crate::support::restate_runtime::grant_tenant_operator;

fn memory_read_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: memory-read-procedure
  description: Procedure that reads contact-scoped graph memory through the Memory service.
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
        - id: recall
          kind: memory_read
          input:
            contact_id: "22222222-2222-2222-2222-222222222222"
            query: "preferred support channel"
            limit: 5
            label_filter:
              - Fact
            max_pii_class: restricted
          ui:
            x: 280
            y: 120
        - id: done
          kind: end
          ui:
            x: 520
            y: 120
      edges:
        - id: start-recall
          from: start
          to: recall
        - id: recall-done
          from: recall
          to: done
      ui:
        layout: dagre
"#
}

fn memory_write_procedure_source() -> &'static str {
    r#"
api_version: moa.artifact/v1
kind: skill
metadata:
  name: memory-write-procedure
  description: Procedure that writes a scoped graph-memory fact through ingestion.
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
        - id: remember
          kind: memory_write
          input:
            contact_id: "22222222-2222-2222-2222-222222222222"
            source_name: procedure memory note
            content: "The customer prefers email updates for support tickets."
            metadata:
              source: procedure-e2e
          ui:
            x: 280
            y: 120
        - id: done
          kind: end
          ui:
            x: 520
            y: 120
      edges:
        - id: start-remember
          from: start
          to: remember
        - id: remember-done
          from: remember
          to: done
      ui:
        layout: dagre
"#
}
