//! Behavior tests for the worker command wire contract shared by execution and workers.

use super::UserReplyDeliveryAck;

#[test]
fn user_reply_delivery_ack_has_exact_strict_wire_values() {
    // Pins: execution and worker input delivery share one stable acknowledgement contract.
    let cases = [
        (UserReplyDeliveryAck::Applied, "\"applied\""),
        (UserReplyDeliveryAck::Replayed, "\"replayed\""),
        (UserReplyDeliveryAck::Conflict, "\"conflict\""),
    ];

    for (ack, expected) in cases {
        assert_eq!(
            serde_json::to_string(&ack).expect("serialize reply delivery acknowledgement"),
            expected
        );
        assert_eq!(
            serde_json::from_str::<UserReplyDeliveryAck>(expected)
                .expect("deserialize reply delivery acknowledgement"),
            ack
        );
    }
    assert!(serde_json::from_str::<UserReplyDeliveryAck>("\"unknown\"").is_err());
}
