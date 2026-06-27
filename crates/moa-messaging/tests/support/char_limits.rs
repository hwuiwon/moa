//! Character-limit test fixtures.

use moa_core::{MessageContent, OutboundMessage};
use unicode_segmentation::UnicodeSegmentation;

/// Builds a simple outbound text message with no buttons or reply target.
pub fn text_message(text: impl Into<String>) -> OutboundMessage {
    OutboundMessage {
        content: MessageContent::Text(text.into()),
        buttons: Vec::new(),
        channel_ref: None,
        reply_to: None,
        ephemeral: false,
    }
}

/// Asserts rendered text chunks preserve input and honor a grapheme-count limit.
pub fn assert_grapheme_chunks(parts: &[String], original: &str, limit: usize) {
    assert!(
        parts.len() >= 2,
        "expected text to split into at least two chunks"
    );
    assert_eq!(
        parts.concat(),
        original,
        "rendered chunks should reconstruct the original text exactly"
    );
    for part in parts {
        assert!(
            part.graphemes(true).count() <= limit,
            "chunk exceeds grapheme limit {limit}: {}",
            part.graphemes(true).count()
        );
        assert!(
            original.contains(part),
            "chunk should be a complete substring of the original text"
        );
    }
}
